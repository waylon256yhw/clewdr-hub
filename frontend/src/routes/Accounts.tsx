import { useEffect, useState } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import {
  Title,
  Badge,
  Button,
  Checkbox,
  Group,
  Modal,
  TextInput,
  NumberInput,
  PasswordInput,
  Textarea,
  Stack,
  Text,
  ActionIcon,
  Skeleton,
  Alert,
  Paper,
  SimpleGrid,
  Progress,
  Select,
  Divider,
  Tooltip,
  Tabs,
  SegmentedControl,
  Switch,
} from "@mantine/core";
import { useForm, type UseFormReturnType } from "@mantine/form";
import { notifications } from "@mantine/notifications";
import { IconPlus, IconEdit, IconTrash, IconRefresh, IconLink, IconFlask, IconStarFilled, IconX } from "@tabler/icons-react";
import {
  listAccounts,
  listProxies,
  createAccount,
  updateAccount,
  deleteAccount,
  probeAllAccounts,
  testAccount,
  startAccountOAuth,
  qk,
  ApiError,
  type Account,
  type MimicryConfig,
  type AccountsListResponse,
  type AccountFailureContext,
  type Proxy,
  type UsageWindow,
} from "../api";
import { formatCost, formatEpochSeconds } from "../lib/format";

const CLI_VERSION_RE = /^\d+\.\d+\.\d+$/;

/**
 * Step 3.5 C5-2: stable color hint for an
 * `AccountFailureContext.action` so the chip / badge color reflects
 * the scheduler verdict without rebuilding a Reason → color table.
 */
function failureBadgeColor(failure: AccountFailureContext): string {
  switch (failure.action.kind) {
    case "terminal_auth":
    case "terminal_disabled":
      return "red";
    case "cooldown":
      return "yellow";
    case "transient_upstream":
      return "orange";
    case "internal_error":
      return "gray";
  }
}

/**
 * Step 3.5 C5-2: multi-line tooltip body for a structured failure.
 * Includes source / stage / upstream HTTP / raw_message — admin-only
 * surface so raw_message is acceptable here.
 */
function buildFailureTooltip(failure: AccountFailureContext): string {
  const parts: string[] = [];
  parts.push(`来源: ${failure.source}`);
  if (failure.stage) parts.push(`阶段: ${failure.stage}`);
  if (failure.upstream_http_status != null) {
    parts.push(`上游 HTTP: ${failure.upstream_http_status}`);
  }
  if (failure.raw_message) {
    parts.push(`原始信息: ${failure.raw_message}`);
  }
  return parts.join("\n");
}

function normalizeAccountType(t: string): string {
  return t.trim().toLowerCase().replace(/[\s-]+/g, "_").replace(/^claude_/, "");
}

function accountTypeColor(t: string): string {
  switch (normalizeAccountType(t)) {
    case "max": return "violet";
    case "enterprise": return "indigo";
    case "pro": return "blue";
    case "free": return "gray";
    default: return "gray";
  }
}

function accountTypeLabel(t: string): string {
  switch (normalizeAccountType(t)) {
    case "max": return "Max";
    case "enterprise": return "Enterprise";
    case "pro": return "Pro";
    case "free": return "Free";
    default: return t;
  }
}

/**
 * Decompose a rate_limit_tier string like `default_claude_max_20x` into
 * `{ plan: "max", multiplier: "20x" }`. Returns null when the string
 * does not match the expected shape; callers should fall back to
 * `account_type` in that case.
 */
function parseRateLimitTier(
  tier: string,
): { plan: string; multiplier: string | null } | null {
  const m = tier.trim().toLowerCase().match(/^default_claude_([a-z]+)(?:_(\d+x))?$/);
  if (!m) return null;
  return { plan: m[1], multiplier: m[2] ?? null };
}

const RECOGNIZED_PLANS = new Set(["max", "pro", "enterprise", "free"]);

/**
 * Display label preferring `rate_limit_tier` (Max 20x / Max 5x / Pro / ...).
 *
 * The tier is only preferred when it carries information beyond
 * `account_type`: a recognized plan family (max/pro/enterprise/free)
 * or a Max-style multiplier suffix. Generic upstream values like
 * `default_claude_ai` parse to plan="ai" with no multiplier — we'd
 * render an "ai" badge that's both wrong and less informative than
 * the derived `account_type` ("Pro"), so fall back in that case.
 */
function planTierLabel(
  rateLimitTier: string | null,
  accountType: string | null,
): string | null {
  if (rateLimitTier) {
    const parsed = parseRateLimitTier(rateLimitTier);
    if (parsed && (RECOGNIZED_PLANS.has(parsed.plan) || parsed.multiplier)) {
      const planLabel = RECOGNIZED_PLANS.has(parsed.plan)
        ? accountTypeLabel(parsed.plan)
        : parsed.plan;
      return parsed.multiplier ? `${planLabel} ${parsed.multiplier}` : planLabel;
    }
  }
  if (accountType) return accountTypeLabel(accountType);
  return null;
}

/** Same precedence as planTierLabel — color tracks the plan family. */
function planTierColor(
  rateLimitTier: string | null,
  accountType: string | null,
): string {
  if (rateLimitTier) {
    const parsed = parseRateLimitTier(rateLimitTier);
    if (parsed && (RECOGNIZED_PLANS.has(parsed.plan) || parsed.multiplier)) {
      return accountTypeColor(parsed.plan);
    }
  }
  if (accountType) return accountTypeColor(accountType);
  return "gray";
}

/**
 * Compute the next monthly renewal anchor. Walks calendar months from
 * `startIso` until the candidate exceeds `now`. When the start day
 * does not exist in the target month (e.g. 1/31 → Feb), clamps to the
 * last day of that month — matches typical subscription billing
 * behavior and `chrono::Months` semantics on the backend.
 */
function nextRenewalDate(startIso: string, now: Date = new Date()): Date | null {
  const start = new Date(startIso);
  if (Number.isNaN(start.getTime())) return null;
  const day = start.getUTCDate();
  const hh = start.getUTCHours();
  const mm = start.getUTCMinutes();
  const ss = start.getUTCSeconds();
  const lastDayOf = (year: number, monthIdx: number) =>
    new Date(Date.UTC(year, monthIdx + 1, 0)).getUTCDate();
  const buildCandidate = (year: number, monthIdx: number) => {
    const clampedDay = Math.min(day, lastDayOf(year, monthIdx));
    return new Date(Date.UTC(year, monthIdx, clampedDay, hh, mm, ss));
  };

  let year = now.getUTCFullYear();
  let monthIdx = now.getUTCMonth();
  let candidate = buildCandidate(year, monthIdx);
  while (candidate.getTime() <= now.getTime()) {
    monthIdx += 1;
    if (monthIdx > 11) {
      monthIdx = 0;
      year += 1;
    }
    candidate = buildCandidate(year, monthIdx);
  }
  return candidate;
}

/** Whole days from now until next renewal. Negative is impossible
 *  by construction (nextRenewalDate always returns a future date),
 *  but treat the boundary day as 0. */
function daysUntilRenewal(startIso: string): number | null {
  const next = nextRenewalDate(startIso);
  if (!next) return null;
  const diffMs = next.getTime() - Date.now();
  return Math.max(0, Math.ceil(diffMs / (24 * 3600 * 1000)));
}

/**
 * Three-band color for the renewal countdown:
 *   * green  — fresh (used < ~1 week into a 30-day cycle, ≥22 days left)
 *   * yellow — mid-cycle (8-21 days left)
 *   * red    — last week of the cycle (≤7 days), pay attention before quota resets
 */
function renewalCountdownColor(days: number): string {
  if (days <= 7) return "red";
  if (days <= 21) return "yellow";
  return "teal";
}

/** Short date "2026-04-10" in Asia/Shanghai. */
function formatSubscriptionDate(iso: string): string | null {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return null;
  return d.toLocaleDateString("zh-CN", {
    timeZone: "Asia/Shanghai",
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
  });
}

function authSourceLabel(source: Account["auth_source"]): string {
  switch (source) {
    case "oauth": return "OAuth";
    case "cookie": return "Cookie";
    case "api_key": return "API Key";
    default: return source;
  }
}

function accountStatusColor(
  status: "active" | "cooling" | "error" | "disabled" | "unconfigured",
): string {
  switch (status) {
    case "active": return "green";
    case "cooling": return "yellow";
    case "error": return "red";
    case "disabled": return "gray";
    case "unconfigured": return "gray";
  }
}

type DisplayState = "active" | "cooling" | "error" | "disabled" | "unconfigured";

/**
 * Derive the badge state from the backend `health` field when present.
 * Falls back to the legacy DB-status + runtime.reset_time heuristic when
 * the account has not been indexed by the pool yet (snapshot/list race).
 */
function resolveDisplayStatus(account: Account): DisplayState {
  if (account.health) {
    switch (account.health.state) {
      case "active": return "active";
      case "cooling_down": return "cooling";
      case "unconfigured": return "unconfigured";
      case "invalid":
        return account.health.kind === "disabled" ? "disabled" : "error";
    }
  }
  if (account.status === "disabled") return "disabled";
  if (account.status === "auth_error") return "error";
  if (account.status === "cooldown") return "cooling";
  if ((account.runtime?.reset_time ?? 0) > Date.now() / 1000) return "cooling";
  return "active";
}

function utilizationColor(v: number): string {
  if (v >= 80) return "red";
  if (v >= 50) return "yellow";
  return "teal";
}

function formatCountdown(epochSecs: number): string {
  const diff = epochSecs - Date.now() / 1000;
  if (diff <= 0) return "已到期";
  const hours = Math.floor(diff / 3600);
  if (hours >= 24) {
    const days = Math.floor(hours / 24);
    const rem = hours % 24;
    return rem > 0 ? `${days}天${rem}小时后` : `${days}天后`;
  }
  const mins = Math.floor((diff % 3600) / 60);
  return hours > 0 ? `${hours}小时${mins}分后` : `${mins}分钟后`;
}

function formatProbeCheckedAt(epochSecs: number | null | undefined): string | null {
  if (!epochSecs) return null;
  return formatEpochSeconds(epochSecs);
}

function WindowRow({ label, window }: { label: string; window: UsageWindow | null | undefined }) {
  if (!window || window.has_reset === null) {
    return (
      <Group justify="space-between" gap="xs">
        <Text size="xs" fw={500} w={80}>{label}</Text>
        <Badge size="xs" color="gray" variant="light">探测中</Badge>
      </Group>
    );
  }
  if (!window.has_reset && window.utilization === null) return null;
  const util = window.utilization ?? 0;
  return (
    <Stack gap={2}>
      <Group justify="space-between" gap="xs">
        <Text size="xs" fw={500}>{label}</Text>
        <Group gap="xs">
          <Text size="xs" c="dimmed">
            {window.resets_at ? formatCountdown(window.resets_at) : "—"}
          </Text>
          <Text size="xs" fw={600} c={utilizationColor(util)}>
            {util.toFixed(0)}%
          </Text>
        </Group>
      </Group>
      <Progress value={util} color={utilizationColor(util)} size="sm" radius="xl" />
    </Stack>
  );
}

function apiKeyExtraHeaderRows(account: Account | null): Array<{ key: string; value: string }> {
  return Object.entries(account?.api_key_extra_headers ?? {}).map(([key, value]) => ({ key, value }));
}

/** Seed the raw-JSON textarea from an account's stored extra body (pretty-printed). */
function apiKeyExtraBodyText(account: Account | null): string {
  const body = account?.api_key_extra_body;
  if (!body || Object.keys(body).length === 0) return "";
  return JSON.stringify(body, null, 2);
}

/**
 * Parse the raw-JSON body textarea into an object, or throw a 400 ApiError.
 * Empty text → `{}` (clear). Must be a JSON object (not array/primitive) and
 * must not override the reserved keys the backend rejects.
 */
function parseApiKeyExtraBody(text: string): Record<string, unknown> {
  const trimmed = text.trim();
  if (!trimmed) return {};
  let parsed: unknown;
  try {
    parsed = JSON.parse(trimmed);
  } catch {
    throw new ApiError(400, "额外请求体必须是合法 JSON");
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new ApiError(400, "额外请求体必须是 JSON 对象");
  }
  const reserved = ["messages", "system"];
  for (const key of Object.keys(parsed)) {
    if (reserved.includes(key.trim().toLowerCase())) {
      throw new ApiError(400, `额外请求体不能覆盖保留字段 “${key}”`);
    }
  }
  return parsed as Record<string, unknown>;
}

/** Seed the mimicry form fields from an account (defaults for a new channel). */
function mimicryInitialValues(account: Account | null) {
  const cfg = account?.mimicry_config ?? null;
  return {
    mimicry_mode: account?.mimicry_mode ?? ("none" as "none" | "third_party"),
    mimicry_auth_header: cfg?.auth_header ?? ("bearer" as "bearer" | "x_api_key"),
    mimicry_cli_version: cfg?.cli_version ?? "",
    // Default strict-system ON for new channels; existing channels keep theirs.
    mimicry_strict_system: cfg?.strict_system ?? true,
    mimicry_extra_beta: (cfg?.extra_beta ?? []).join(", "),
  };
}

function AccountCard({
  account,
  probing,
  probeError,
  onEdit,
  onDelete,
}: {
  account: Account;
  probing: boolean;
  probeError?: string;
  onEdit: () => void;
  onDelete: () => void;
}) {
  const rt = account.runtime;
  const isApiKey = account.auth_source === "api_key";
  const extraHeaderRows = apiKeyExtraHeaderRows(account);
  const displayStatus = resolveDisplayStatus(account);
  const isProbing = account.health?.probing ?? probing;
  const effectiveProbeError = account.health?.last_probe_error ?? probeError;
  const probeCheckedAt = formatProbeCheckedAt(rt?.resets_last_checked_at);
  // Step 3.5 C5-2: surface structured failure context when available.
  // Only the `invalid` health variant carries `last_failure`; the
  // backend gates on live state so we never see it for active /
  // cooling accounts.
  const lastFailure =
    account.health?.state === "invalid" ? account.health.last_failure : null;
  const lastFailureTooltip = lastFailure
    ? buildFailureTooltip(lastFailure)
    : null;
  const testMut = useMutation({
    mutationFn: () => testAccount(account.id),
    onSuccess: (resp) => {
      if (resp.success) {
        notifications.show({ message: `测试通过 (${resp.latency_ms}ms)`, color: "green" });
      } else {
        notifications.show({
          title: "测试失败",
          message: resp.error ?? `HTTP ${resp.http_status}`,
          color: "red",
          autoClose: 8000,
        });
      }
    },
    onError: (e) =>
      notifications.show({
        message: e instanceof ApiError ? e.message : "测试请求失败",
        color: "red",
      }),
  });
  return (
    <Paper withBorder shadow="xs" radius="md" p="md">
      <Group justify="space-between" mb="xs">
        <Group gap={6}>
          <Text fw={600}>{account.name}</Text>
          {account.drain_first && (
            <Tooltip label="优先消耗">
              <IconStarFilled size={14} color="var(--mantine-color-orange-6)" />
            </Tooltip>
          )}
        </Group>
        <Group gap={4}>
          <Tooltip label="测试 /v1/messages">
            <ActionIcon
              variant="subtle"
              size="sm"
              color="cyan"
              loading={testMut.isPending}
              onClick={() => testMut.mutate()}
            >
              <IconFlask size={14} />
            </ActionIcon>
          </Tooltip>
          <ActionIcon variant="subtle" size="sm" onClick={onEdit}>
            <IconEdit size={14} />
          </ActionIcon>
          <ActionIcon variant="subtle" size="sm" color="red" onClick={onDelete}>
            <IconTrash size={14} />
          </ActionIcon>
        </Group>
      </Group>

      <Group gap="xs" mb="xs">
        <Badge color={accountStatusColor(displayStatus)} variant="light" size="sm">
          {displayStatus}
        </Badge>
        {isProbing && !isApiKey && <Badge color="blue" variant="light" size="sm">probing</Badge>}
        <Badge color="dark" variant="outline" size="sm">{authSourceLabel(account.auth_source)}</Badge>
        {(() => {
          const label = planTierLabel(account.rate_limit_tier, account.account_type);
          if (!label) return null;
          // Tooltip body: stash billing_type here when available — too
          // niche for a dedicated chip but useful when triaging
          // payment-method-specific issues (Play Store family plans,
          // self-serve Stripe, etc.).
          const tooltipParts: string[] = [];
          if (account.rate_limit_tier) {
            tooltipParts.push(`tier: ${account.rate_limit_tier}`);
          }
          if (account.billing_type) {
            tooltipParts.push(`billing: ${account.billing_type}`);
          }
          const badge = (
            <Badge
              color={planTierColor(account.rate_limit_tier, account.account_type)}
              variant="light"
              size="sm"
            >
              {label}
            </Badge>
          );
          return tooltipParts.length > 0 ? (
            <Tooltip label={tooltipParts.join("\n")} multiline withArrow>
              {badge}
            </Tooltip>
          ) : (
            badge
          );
        })()}
        {account.proxy_name && (
          <Badge color="grape" variant="light" size="sm">
            代理: {account.proxy_name}
          </Badge>
        )}
        {/*
          Step 3.5 C5-2: structured failure chip. Reads
          `last_failure.normalized_reason_type` (stable snake_case
          string) for the visible label; the tooltip carries the
          richer source / stage / upstream HTTP / raw_message context.
          Hidden whenever the live health is not invalid, so a
          stale row never leaks past the backend's gating.
        */}
        {lastFailure && lastFailureTooltip && (
          <Tooltip label={lastFailureTooltip} multiline w={320} withArrow>
            <Badge color={failureBadgeColor(lastFailure)} variant="filled" size="sm">
              {lastFailure.normalized_reason_type}
            </Badge>
          </Tooltip>
        )}
      </Group>

      {account.email && (
        <Text size="xs" c="dimmed" mb="xs" lineClamp={1}>{account.email}</Text>
      )}

      {account.subscription_created_at && !isApiKey && (() => {
        const days = daysUntilRenewal(account.subscription_created_at);
        if (days === null) return null;
        const startLabel = formatSubscriptionDate(account.subscription_created_at);
        const body = (
          <Text size="xs" c="dimmed" mb="xs">
            剩余{" "}
            <Text component="span" fw={700} c={renewalCountdownColor(days)}>
              {days <= 0 ? "<1 天" : `${days} 天`}
            </Text>
          </Text>
        );
        return startLabel ? (
          <Tooltip label={`订阅创建于 ${startLabel}`} withArrow>
            {body}
          </Tooltip>
        ) : (
          body
        );
      })()}

      {probeCheckedAt && !isApiKey && (
        <Text size="xs" c="dimmed" mb="xs">探测更新时间: {probeCheckedAt}</Text>
      )}

      {/*
        invalid_reason / last_error come straight from the DB row. The
        list handler loads accounts and pool state in two separate calls,
        so during the collect→do_flush window the DB row can still carry
        a stale invalid_reason / last_error even though the pool has
        already reclassified the account. Show these strings only when
        the snapshot agrees ("invalid"), or when we never got a snapshot
        (health missing) and have to trust the DB. This keeps the red
        text from contradicting the green/yellow badge above.
      */}
      {account.invalid_reason &&
        !lastFailure &&
        (!account.health || account.health.state === "invalid") && (
          <Text size="xs" c="red" mb="xs">{account.invalid_reason}</Text>
        )}

      {effectiveProbeError && !isApiKey && (
        <Text size="xs" c="orange" mb="xs">探测错误: {effectiveProbeError}</Text>
      )}

      {account.last_error &&
        !lastFailure &&
        (!account.health || account.health.state === "invalid") && (
          <Text size="xs" c="orange" mb="xs">OAuth: {account.last_error}</Text>
        )}

      {isApiKey ? (
        <Stack gap={4}>
          <Group justify="space-between" gap="xs">
            <Text size="xs" fw={500}>累计消耗</Text>
            <Text size="xs" fw={600}>{formatCost(account.total_cost_nanousd)}</Text>
          </Group>
          {account.api_key_base_url && (
            <Text size="xs" c="dimmed" lineClamp={1}>
              Base URL: {account.api_key_base_url}
            </Text>
          )}
          {extraHeaderRows.length > 0 && (
            <Group justify="space-between" gap="xs">
              <Text size="xs" fw={500}>额外请求头</Text>
              <Text size="xs" c="dimmed">{extraHeaderRows.length} 项</Text>
            </Group>
          )}
        </Stack>
      ) : (
        <>
          <Divider my="xs" />

          <Stack gap="xs">
            <WindowRow label="5h 会话" window={rt?.session} />
            <WindowRow label="7d 总量" window={rt?.weekly} />
            <WindowRow label="7d Sonnet" window={rt?.weekly_sonnet} />
            <WindowRow label="7d Opus" window={rt?.weekly_opus} />
          </Stack>
        </>
      )}
    </Paper>
  );
}

interface FormValues {
  name: string;
  rr_order: number;
  max_slots: number;
  proxy_id: string | null;
  drain_first: boolean;
  cookie_blob: string;
  oauth_callback_input: string;
  /** ApiKey base URL. Empty when not editing api_key. */
  api_key_base_url: string;
  /** ApiKey secret (raw). Never echoed back from server — empty on edit means "keep existing". */
  api_key_secret: string;
  /**
   * Editable list of {key, value} pairs for ApiKey extra headers.
   * Empty rows are dropped on submit; an empty list submitted as
   * `Some({})` explicitly clears server-side extras.
   */
  api_key_extra_headers: Array<{ key: string; value: string }>;
  /**
   * Raw JSON object (as text) shallow-merged over the ApiKey request body,
   * e.g. `{"models": ["claude-opus-4-7"]}`. Validated for JSON-object validity
   * on submit only. Empty string means "no injection" / clear.
   */
  api_key_extra_body: string;
  /** Two-tier mimicry mode (api_key only). */
  mimicry_mode: "none" | "third_party";
  /** Auth header form for the third-party cloak. */
  mimicry_auth_header: "bearer" | "x_api_key";
  /** Empty = inherit the global tp_cloak_cli_version. */
  mimicry_cli_version: string;
  mimicry_strict_system: boolean;
  /** Comma/newline-separated extra beta tokens. */
  mimicry_extra_beta: string;
}

/**
 * Step 5 / C12 — ApiKey credential editor.
 *
 * Three sub-controls:
 *   1. Base URL — admin-supplied, safe to echo. Defaults to
 *      `https://api.anthropic.com/` on create. Re-normalized server-side
 *      via `normalize_api_key_base_url` so trailing-slash / `/v1` variants
 *      all collapse to the canonical shape.
 *   2. Secret — password input. On edit it stays empty by default and
 *      "leave empty to keep" semantics apply (mirror of the cookie /
 *      OAuth credential-replacement flow).
 *   3. Extra headers — KV widget. Empty rows are dropped on submit; an
 *      empty list submitted as `Some({})` explicitly clears server-side
 *      extras.
 *
 * The reserved-name set (`x-api-key`, `authorization`, etc.) is
 * validated server-side at write time and surfaced as a 400 — the
 * editor doesn't pre-block input so the error message is the single
 * source of truth.
 */
function ApiKeyTabPanel({
  form,
  editing,
  markExtrasDirty,
  markBodyDirty,
}: {
  form: UseFormReturnType<FormValues>;
  editing: Account | null;
  /** Marks the KV list as changed so edit submit can send replace/clear. */
  markExtrasDirty: () => void;
  /** Marks the raw-JSON body as changed so edit submit can send replace/clear. */
  markBodyDirty: () => void;
}) {
  const rows = form.getValues().api_key_extra_headers;
  // Sealed view by default when editing an account that already has
  // headers. Click "编辑" or add a row to drop into the inline KV editor.
  const [unlocked, setUnlocked] = useState(
    () => apiKeyExtraHeaderRows(editing).length === 0,
  );
  // Local mirrors for the mimicry controls. The form is uncontrolled, so these
  // drive display + the mode-based conditional rendering; each change also
  // writes through to the form so submit reads the current values.
  const initial = form.getValues();
  const [mMode, setMMode] = useState<"none" | "third_party">(initial.mimicry_mode);
  const [mAuth, setMAuth] = useState<"bearer" | "x_api_key">(initial.mimicry_auth_header);
  const [mVer, setMVer] = useState(initial.mimicry_cli_version);
  const [mStrict, setMStrict] = useState(initial.mimicry_strict_system);
  const [mBeta, setMBeta] = useState(initial.mimicry_extra_beta);
  const [extraBodyText, setExtraBodyText] = useState(initial.api_key_extra_body);
  const addRow = () => {
    form.setFieldValue("api_key_extra_headers", [
      ...form.getValues().api_key_extra_headers,
      { key: "", value: "" },
    ]);
    markExtrasDirty();
    setUnlocked(true);
  };
  return (
    <Stack>
      <TextInput
        label="基础 URL"
        placeholder="https://api.anthropic.com/"
        required={!editing}
        key={form.key("api_key_base_url")}
        {...form.getInputProps("api_key_base_url")}
      />
      <PasswordInput
        label={editing ? "API 密钥（留空保留原值）" : "API 密钥"}
        placeholder="sk-ant-..."
        required={!editing}
        key={form.key("api_key_secret")}
        {...form.getInputProps("api_key_secret")}
      />
      <Stack gap={6}>
        <Group justify="space-between" align="center">
          <Text size="sm" fw={500}>
            额外请求头（可选）
          </Text>
          {unlocked ? (
            <Button
              type="button"
              size="xs"
              variant="light"
              leftSection={<IconPlus size={14} />}
              onClick={addRow}
            >
              添加
            </Button>
          ) : (
            <Button
              type="button"
              size="xs"
              variant="subtle"
              leftSection={<IconEdit size={14} />}
              onClick={() => setUnlocked(true)}
            >
              编辑
            </Button>
          )}
        </Group>
        {unlocked ? (
          <>
            <Text size="xs" c="dimmed">可选；留空表示不附加额外请求头。</Text>
            {rows.map((row, idx) => (
              <Group key={idx} gap="xs" align="flex-end" wrap="nowrap">
                <TextInput
                  flex={1}
                  placeholder="header name"
                  value={row.key}
                  onChange={(e) => {
                    const next = [...form.getValues().api_key_extra_headers];
                    next[idx] = { ...next[idx], key: e.currentTarget.value };
                    form.setFieldValue("api_key_extra_headers", next);
                    markExtrasDirty();
                  }}
                />
                <TextInput
                  flex={2}
                  placeholder="value"
                  value={row.value}
                  onChange={(e) => {
                    const next = [...form.getValues().api_key_extra_headers];
                    next[idx] = { ...next[idx], value: e.currentTarget.value };
                    form.setFieldValue("api_key_extra_headers", next);
                    markExtrasDirty();
                  }}
                />
                <ActionIcon
                  variant="subtle"
                  color="red"
                  aria-label="移除该行"
                  onClick={() => {
                    const next = form
                      .getValues()
                      .api_key_extra_headers.filter((_, i) => i !== idx);
                    form.setFieldValue("api_key_extra_headers", next);
                    markExtrasDirty();
                  }}
                >
                  <IconX size={14} />
                </ActionIcon>
              </Group>
            ))}
          </>
        ) : (
          <Stack gap={4}>
            {rows.map((row, idx) => (
              <Text
                key={idx}
                size="xs"
                c="dimmed"
                style={{
                  fontFamily: "var(--mantine-font-family-monospace)",
                  wordBreak: "break-all",
                }}
              >
                {row.key}: {row.value}
              </Text>
            ))}
          </Stack>
        )}
      </Stack>

      <Textarea
        label="额外请求体 JSON（可选）"
        description={
          '浅合并进出站请求体（仅 /v1/messages）。必须是 JSON 对象；键 messages / system 不可覆盖。' +
          '例：{"models": ["claude-opus-4-7"]}'
        }
        placeholder={'{\n  "models": ["claude-opus-4-7"]\n}'}
        autosize
        minRows={3}
        maxRows={12}
        styles={{ input: { fontFamily: "var(--mantine-font-family-monospace)" } }}
        value={extraBodyText}
        onChange={(e) => {
          const v = e.currentTarget.value;
          setExtraBodyText(v);
          form.setFieldValue("api_key_extra_body", v);
          markBodyDirty();
        }}
      />

      <Divider label="中转伪装 (Mimicry)" labelPosition="left" />
      <Stack gap="xs">
        <SegmentedControl
          fullWidth
          value={mMode}
          onChange={(v) => {
            const m = v as "none" | "third_party";
            setMMode(m);
            form.setFieldValue("mimicry_mode", m);
          }}
          data={[
            { label: "关闭", value: "none" },
            { label: "Claude Cloak", value: "third_party" },
          ]}
        />
        {mMode === "third_party" && (
          <Stack gap="sm" pl="xs">
            <div>
              <Text size="sm" fw={500} mb={4}>
                认证头
              </Text>
              <SegmentedControl
                value={mAuth}
                onChange={(v) => {
                  const a = v as "bearer" | "x_api_key";
                  setMAuth(a);
                  form.setFieldValue("mimicry_auth_header", a);
                }}
                data={[
                  { label: "Authorization: Bearer", value: "bearer" },
                  { label: "x-api-key", value: "x_api_key" },
                ]}
              />
            </div>
            <TextInput
              label="渠道覆盖 CLI 版本"
              description="留空继承全局默认"
              placeholder="继承全局默认"
              value={mVer}
              onChange={(e) => {
                setMVer(e.currentTarget.value);
                form.setFieldValue("mimicry_cli_version", e.currentTarget.value);
              }}
            />
            <Switch
              label="严格 system 模式"
              description="将客户端 system 下沉为首条 user 消息，wire 上只保留 Claude Code 身份"
              checked={mStrict}
              onChange={(e) => {
                setMStrict(e.currentTarget.checked);
                form.setFieldValue("mimicry_strict_system", e.currentTarget.checked);
              }}
            />
            <Textarea
              label="额外 anthropic-beta（可选）"
              description="逗号或换行分隔；用于中转站要求的额外 beta token"
              autosize
              minRows={1}
              value={mBeta}
              onChange={(e) => {
                setMBeta(e.currentTarget.value);
                form.setFieldValue("mimicry_extra_beta", e.currentTarget.value);
              }}
            />
          </Stack>
        )}
      </Stack>
    </Stack>
  );
}

function AccountFormModal({
  opened,
  onClose,
  editing,
  proxies,
}: {
  opened: boolean;
  onClose: () => void;
  editing: Account | null;
  proxies: Proxy[];
}) {
  const queryClient = useQueryClient();
  const [tab, setTab] = useState<"oauth" | "cookie" | "api_key">(
    editing?.auth_source === "cookie"
      ? "cookie"
      : editing?.auth_source === "api_key"
        ? "api_key"
        : "oauth",
  );
  const [authUrl, setAuthUrl] = useState("");
  const [oauthState, setOauthState] = useState("");
  // The backend's update contract is tri-state:
  // omitted = keep, {} = clear, map = replace. Track whether the
  // admin touched the KV list so ordinary edits don't rewrite it.
  const [extraHeadersDirty, setExtraHeadersDirty] = useState(false);
  // Same tri-state contract as extra headers for the raw-JSON body field.
  const [extraBodyDirty, setExtraBodyDirty] = useState(false);
  const form = useForm<FormValues>({
    mode: "uncontrolled",
    initialValues: {
      name: editing?.name ?? "",
      rr_order: editing?.rr_order ?? 0,
      max_slots: 5,
      proxy_id: editing?.proxy_id ? String(editing.proxy_id) : null,
      drain_first: editing?.drain_first ?? false,
      cookie_blob: "",
      oauth_callback_input: "",
      api_key_base_url: editing?.api_key_base_url ?? "",
      api_key_secret: "",
      api_key_extra_headers: apiKeyExtraHeaderRows(editing),
      api_key_extra_body: apiKeyExtraBodyText(editing),
      ...mimicryInitialValues(editing),
    },
  });

  // The modal is keyed on `editing?.id`, so id changes already remount with
  // fresh `initialValues`. This effect covers the residual case: reopening the
  // SAME id after its data was refreshed (same key → no remount → re-sync
  // here). Kept off the key so a background refresh can't wipe a mid-edit form.
  useEffect(() => {
    setTab(
      editing?.auth_source === "cookie"
        ? "cookie"
        : editing?.auth_source === "api_key"
          ? "api_key"
          : "oauth",
    );
    setAuthUrl("");
    setOauthState("");
    setExtraHeadersDirty(false);
    setExtraBodyDirty(false);
    form.setValues({
      name: editing?.name ?? "",
      rr_order: editing?.rr_order ?? 0,
      max_slots: 5,
      proxy_id: editing?.proxy_id ? String(editing.proxy_id) : null,
      drain_first: editing?.drain_first ?? false,
      cookie_blob: "",
      oauth_callback_input: "",
      api_key_base_url: editing?.api_key_base_url ?? "",
      api_key_secret: "",
      api_key_extra_headers: apiKeyExtraHeaderRows(editing),
      api_key_extra_body: apiKeyExtraBodyText(editing),
      ...mimicryInitialValues(editing),
    });
  }, [editing]); // eslint-disable-line react-hooks/exhaustive-deps

  const oauthStartMutation = useMutation({
    mutationFn: () => startAccountOAuth(),
    onSuccess: async (resp) => {
      setAuthUrl(resp.auth_url);
      setOauthState(resp.state);
      try {
        await navigator.clipboard.writeText(resp.auth_url);
        notifications.show({ message: "鉴权 URL 已复制", color: "green" });
      } catch {
        notifications.show({ message: "鉴权 URL 已生成", color: "green" });
      }
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "生成鉴权 URL 失败", color: "red" }),
  });

  const mutation = useMutation({
    mutationFn: async (values: FormValues) => {
      const name = values.name.trim();
      const proxyId = values.proxy_id ? Number(values.proxy_id) : null;
      const cookieBlob = tab === "cookie" ? values.cookie_blob.trim() : "";
      const oauthInput = tab === "oauth" ? values.oauth_callback_input.trim() : "";
      const scopedOauthState = tab === "oauth" ? oauthState : "";

      // ApiKey payload normalization. Drop pair rows where the key is
      // blank — those are placeholders from the "add row" button. A
      // pair with empty value but non-empty key is still kept so the
      // backend can return a clear "header value required" error.
      const apiKeyBaseUrl = tab === "api_key" ? values.api_key_base_url.trim() : "";
      const apiKeySecret = tab === "api_key" ? values.api_key_secret.trim() : "";
      const apiKeyExtraHeadersObj: Record<string, string> | undefined =
        tab === "api_key"
          ? Object.fromEntries(
              values.api_key_extra_headers
                .map((r) => [r.key.trim(), r.value] as const)
                .filter(([k]) => k.length > 0),
            )
          : undefined;
      // Raw-JSON body injection. Validated for JSON-object validity here (throws
      // a 400 on bad input); `{}` means "no injection" / clear.
      const apiKeyExtraBodyObj: Record<string, unknown> | undefined =
        tab === "api_key" ? parseApiKeyExtraBody(values.api_key_extra_body) : undefined;

      if (!name) throw new ApiError(400, "名称必填");
      if (!editing && tab === "cookie" && !cookieBlob) throw new ApiError(400, "新账号必须提供 Cookie");
      if (!editing && tab === "oauth" && !oauthInput) throw new ApiError(400, "请粘贴 Callback URL 或 Code");
      if (!editing && tab === "api_key" && !apiKeyBaseUrl) throw new ApiError(400, "新账号必须提供 API 基础 URL");
      if (!editing && tab === "api_key" && !apiKeySecret) throw new ApiError(400, "新账号必须提供 API 密钥");

      // Two-tier mimicry (api_key tab only). `none` sends no config; a
      // `third_party` channel serializes the cloak knobs.
      const mimicryMode = tab === "api_key" ? values.mimicry_mode : undefined;
      const mimicryCliVersion = values.mimicry_cli_version.trim();
      if (
        tab === "api_key" &&
        values.mimicry_mode === "third_party" &&
        mimicryCliVersion &&
        !CLI_VERSION_RE.test(mimicryCliVersion)
      ) {
        throw new ApiError(400, "渠道覆盖 CLI 版本必须是 x.y.z，或留空继承全局默认");
      }
      const mimicryConfig: MimicryConfig | undefined =
        tab === "api_key" && values.mimicry_mode === "third_party"
          ? {
              auth_header: values.mimicry_auth_header,
              cli_version: mimicryCliVersion || null,
              strict_system: values.mimicry_strict_system,
              extra_beta: values.mimicry_extra_beta
                .split(/[\n,]/)
                .map((s) => s.trim())
                .filter(Boolean),
            }
          : undefined;
      // On edit within the api_key tab we let the user submit without
      // re-entering the secret (empty = keep existing, mirror of the
      // cookie/oauth flow). Switching INTO api_key from a different
      // auth_source requires both base_url and secret, surfaced by the
      // backend as a 400.

      if (editing) {
        const body: Record<string, unknown> = {};
        if (name !== editing.name) body.name = name;
        if (values.rr_order !== editing.rr_order) body.rr_order = values.rr_order;
        if ((editing.proxy_id ?? null) !== proxyId) body.proxy_id = proxyId ?? 0;
        if (values.drain_first !== editing.drain_first) body.drain_first = values.drain_first;
        if (cookieBlob) body.cookie_blob = cookieBlob;
        if (oauthInput) body.oauth_callback_input = oauthInput;
        if (scopedOauthState) body.oauth_state = scopedOauthState;
        if (tab === "api_key") {
          // Send the api_key fields only on the api_key tab so the
          // backend's "submit exactly one credential kind" guard fires
          // correctly when the user switches credential kinds.
          if (apiKeyBaseUrl && apiKeyBaseUrl !== (editing.api_key_base_url ?? "")) {
            body.api_key_base_url = apiKeyBaseUrl;
          }
          if (apiKeySecret) body.api_key_secret = apiKeySecret;
          // Only forward this tri-state field after the KV editor was
          // touched; otherwise ordinary edits should preserve headers.
          if (extraHeadersDirty && apiKeyExtraHeadersObj !== undefined) {
            body.api_key_extra_headers = apiKeyExtraHeadersObj;
          }
          // Same tri-state as headers: forward only after the textarea was
          // touched. Empty text → `{}` explicitly clears the stored body.
          if (extraBodyDirty && apiKeyExtraBodyObj !== undefined) {
            body.api_key_extra_body = apiKeyExtraBodyObj;
          }
          // Always forward the mimicry mode on the api_key tab (backend runs
          // its mimicry update only when the field is present, and treats it
          // idempotently). Config rides along only for a third_party channel.
          body.mimicry_mode = mimicryMode;
          if (mimicryConfig) body.mimicry_config = mimicryConfig;
        }
        return updateAccount(editing.id, body);
      }
      return createAccount({
        name,
        max_slots: values.max_slots,
        proxy_id: proxyId ?? undefined,
        drain_first: values.drain_first,
        auth_source: tab,
        cookie_blob: cookieBlob || undefined,
        oauth_callback_input: oauthInput || undefined,
        oauth_state: scopedOauthState || undefined,
        api_key_base_url: apiKeyBaseUrl || undefined,
        api_key_secret: apiKeySecret || undefined,
        api_key_extra_headers:
          apiKeyExtraHeadersObj && Object.keys(apiKeyExtraHeadersObj).length > 0
            ? apiKeyExtraHeadersObj
            : undefined,
        api_key_extra_body:
          apiKeyExtraBodyObj && Object.keys(apiKeyExtraBodyObj).length > 0
            ? apiKeyExtraBodyObj
            : undefined,
        mimicry_mode: mimicryMode,
        mimicry_config: mimicryConfig,
      });
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: qk.accounts });
      queryClient.invalidateQueries({ queryKey: qk.overview });
      notifications.show({ message: editing ? "账号已更新" : "账号已创建", color: "green" });
      form.reset();
      setAuthUrl("");
      setOauthState("");
      onClose();
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "操作失败", color: "red" }),
  });

  return (
    <Modal opened={opened} onClose={onClose} title={editing ? "编辑账号" : "新建账号"}>
      <form onSubmit={form.onSubmit((v) => mutation.mutate(v))}>
        <Stack>
          <TextInput label="名称" required key={form.key("name")} {...form.getInputProps("name")} />
          {editing && <NumberInput label="轮询顺序" key={form.key("rr_order")} {...form.getInputProps("rr_order")} />}
          {!editing && <NumberInput label="最大并发" min={1} key={form.key("max_slots")} {...form.getInputProps("max_slots")} />}
          <Select
            label="代理"
            data={proxies.map((proxy) => ({
              value: String(proxy.id),
              label: proxy.name,
            }))}
            clearable
            placeholder="不使用代理"
            key={form.key("proxy_id")}
            {...form.getInputProps("proxy_id")}
          />
          <Checkbox
            label="优先消耗"
            description="打开后此账号会被优先选中"
            key={form.key("drain_first")}
            {...form.getInputProps("drain_first", { type: "checkbox" })}
          />
          <Tabs
            value={tab}
            keepMounted={false}
            onChange={(value) => {
              const nextTab = (value as "oauth" | "cookie" | "api_key") ?? "oauth";
              setTab(nextTab);
              // Clear the OTHER credential kinds' field state when
              // switching tabs so a half-typed cookie doesn't leak
              // into an api_key submission.
              if (nextTab !== "oauth") {
                form.setFieldValue("oauth_callback_input", "");
                setAuthUrl("");
                setOauthState("");
              }
              if (nextTab !== "cookie") {
                form.setFieldValue("cookie_blob", "");
              }
              if (nextTab !== "api_key") {
                form.setFieldValue("api_key_secret", "");
                form.setFieldValue("api_key_extra_headers", []);
                setExtraHeadersDirty(false);
                form.setFieldValue("api_key_extra_body", "");
                setExtraBodyDirty(false);
              }
            }}
          >
            <Tabs.List>
              <Tabs.Tab value="oauth">OAuth Token</Tabs.Tab>
              <Tabs.Tab value="cookie">Cookie</Tabs.Tab>
              <Tabs.Tab value="api_key">API Key</Tabs.Tab>
            </Tabs.List>
            <Tabs.Panel value="oauth" pt="md">
              <Stack>
                <Group justify="space-between" align="flex-start">
                  <Text size="sm" c="dimmed" maw={420}>
                    先生成鉴权 URL 并在浏览器完成授权，再把完整 callback URL 或单独 code 粘贴回来。
                  </Text>
                  <Button
                    type="button"
                    size="xs"
                    variant="light"
                    leftSection={<IconLink size={14} />}
                    loading={oauthStartMutation.isPending}
                    onClick={() => oauthStartMutation.mutate()}
                  >
                    生成并复制 URL
                  </Button>
                </Group>
                {authUrl && (
                  <TextInput
                    label="鉴权 URL"
                    value={authUrl}
                    readOnly
                    styles={{
                      input: {
                        overflow: "hidden",
                        textOverflow: "ellipsis",
                        whiteSpace: "nowrap",
                      },
                    }}
                  />
                )}
                <Textarea
                  label={editing ? "Callback URL / Code（可选）" : "Callback URL / Code"}
                  placeholder="粘贴完整 callback URL 或单独 code"
                  autosize
                  minRows={3}
                  key={form.key("oauth_callback_input")}
                  {...form.getInputProps("oauth_callback_input")}
                />
              </Stack>
            </Tabs.Panel>
            <Tabs.Panel value="cookie" pt="md">
              <Textarea
                label={editing ? "替换 Cookie（可选）" : "Cookie"}
                placeholder="粘贴 Cookie..."
                autosize
                minRows={3}
                key={form.key("cookie_blob")}
                {...form.getInputProps("cookie_blob")}
              />
            </Tabs.Panel>
            <Tabs.Panel value="api_key" pt="md">
              <ApiKeyTabPanel
                form={form}
                editing={editing}
                markExtrasDirty={() => setExtraHeadersDirty(true)}
                markBodyDirty={() => setExtraBodyDirty(true)}
              />
            </Tabs.Panel>
          </Tabs>
          <Group justify="flex-end">
            <Button variant="default" onClick={onClose}>取消</Button>
            <Button type="submit" loading={mutation.isPending}>
              {editing ? "保存" : "创建"}
            </Button>
          </Group>
        </Stack>
      </form>
    </Modal>
  );
}

function DeleteModal({
  account,
  onClose,
}: {
  account: Account | null;
  onClose: () => void;
}) {
  const queryClient = useQueryClient();
  const mutation = useMutation({
    mutationFn: () => deleteAccount(account!.id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: qk.accounts });
      queryClient.invalidateQueries({ queryKey: qk.overview });
      notifications.show({ message: "账号已删除", color: "green" });
      onClose();
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "删除失败", color: "red" }),
  });

  return (
    <Modal opened={!!account} onClose={onClose} title="删除账号">
      <Stack>
        <Text>
          确定要删除账号 <strong>{account?.name}</strong>？此操作不可恢复。
        </Text>
        <Group justify="flex-end">
          <Button variant="default" onClick={onClose}>取消</Button>
          <Button color="red" loading={mutation.isPending} onClick={() => mutation.mutate()}>
            删除
          </Button>
        </Group>
      </Stack>
    </Modal>
  );
}

export default function Accounts() {
  const queryClient = useQueryClient();
  const { data, isLoading, error } = useQuery({
    queryKey: qk.accounts,
    queryFn: listAccounts,
    refetchInterval: (query) => {
      const items = query.state.data?.items ?? [];
      const anyProbing = items.some((item) => item.health?.probing);
      return anyProbing ? 3000 : 30_000;
    },
  });
  const { data: proxiesData } = useQuery({
    queryKey: qk.proxies,
    queryFn: listProxies,
  });
  const [formOpened, setFormOpened] = useState(false);
  const [editing, setEditing] = useState<Account | null>(null);
  const [deleting, setDeleting] = useState<Account | null>(null);

  const probeMut = useMutation({
    mutationFn: probeAllAccounts,
    onSuccess: (resp) => {
      notifications.show({ message: "已触发全量探测", color: "green" });
      const probingSet = new Set(resp.probing_ids);
      // Optimistic patch: items already in cache with populated `health` get
      // an instant probing badge so the banner / 3s polling cadence kicks in
      // before the next refetch.
      queryClient.setQueryData(qk.accounts, (old: AccountsListResponse | undefined) => {
        if (!old) return old;
        return {
          ...old,
          items: old.items.map((item) =>
            probingSet.has(item.id) && item.health
              ? { ...item, health: { ...item.health, probing: true } }
              : item,
          ),
        };
      });
      // Safety net for rows whose `health` isn't cached yet (just-created
      // accounts the pool hasn't indexed into its snapshot). Server has
      // already run begin_probe; an immediate refetch surfaces probing=true
      // for those rows so the banner / polling cadence converge there too.
      queryClient.invalidateQueries({ queryKey: qk.accounts });
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "探测失败", color: "red" }),
  });

  if (isLoading) return <Skeleton height={300} radius="md" />;
  if (error) {
    return (
      <Alert color="red" title="Failed to load accounts">
        {String(error)}
      </Alert>
    );
  }

  const accounts = data?.items ?? [];
  const proxies = proxiesData?.items ?? [];
  const probingIds = new Set(
    accounts.filter((a) => a.auth_source !== "api_key" && a.health?.probing).map((a) => a.id),
  );

  const openCreate = () => {
    setEditing(null);
    setFormOpened(true);
  };
  const openEdit = (a: Account) => {
    setEditing(a);
    setFormOpened(true);
  };

  return (
    <>
      <Group justify="space-between" mb="md">
        <Title order={3}>账号池</Title>
        <Group gap="xs">
          <Tooltip label="探测所有账号用量">
            <ActionIcon variant="default" loading={probeMut.isPending} onClick={() => probeMut.mutate()}>
              <IconRefresh size={16} />
            </ActionIcon>
          </Tooltip>
          <Button leftSection={<IconPlus size={16} />} onClick={openCreate}>
            添加账号
          </Button>
        </Group>
      </Group>

      {probingIds.size > 0 && (
        <Alert color="blue" mb="md">
          正在探测 {probingIds.size}/{accounts.length} 个账号...
        </Alert>
      )}

      {accounts.length === 0 ? (
        <Text c="dimmed">暂无账号，点击上方按钮添加。</Text>
      ) : (
        <SimpleGrid cols={{ base: 1, md: 2, xl: 3 }} style={{ alignItems: "start" }}>
          {accounts.map((a) => (
            <AccountCard
              key={a.id}
              account={a}
              probing={probingIds.has(a.id)}
              onEdit={() => openEdit(a)}
              onDelete={() => setDeleting(a)}
            />
          ))}
        </SimpleGrid>
      )}

      <AccountFormModal
        key={editing?.id ?? "new"}
        opened={formOpened}
        onClose={() => setFormOpened(false)}
        editing={editing}
        proxies={proxies}
      />
      <DeleteModal account={deleting} onClose={() => setDeleting(null)} />
    </>
  );
}
