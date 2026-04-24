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
} from "@mantine/core";
import { useForm } from "@mantine/form";
import { notifications } from "@mantine/notifications";
import { IconPlus, IconEdit, IconTrash, IconRefresh, IconLink, IconFlask, IconStarFilled } from "@tabler/icons-react";
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
  type AccountsListResponse,
  type Proxy,
  type UsageWindow,
} from "../api";
import { formatEpochSeconds } from "../lib/format";

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

function authSourceLabel(source: Account["auth_source"]): string {
  switch (source) {
    case "oauth": return "OAuth";
    case "cookie": return "Cookie";
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
  const displayStatus = resolveDisplayStatus(account);
  const isProbing = account.health?.probing ?? probing;
  const effectiveProbeError = account.health?.last_probe_error ?? probeError;
  const probeCheckedAt = formatProbeCheckedAt(rt?.resets_last_checked_at);
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
        {isProbing && <Badge color="blue" variant="light" size="sm">probing</Badge>}
        <Badge color="dark" variant="outline" size="sm">{authSourceLabel(account.auth_source)}</Badge>
        {account.account_type && (
          <Badge color={accountTypeColor(account.account_type)} variant="light" size="sm">
            {accountTypeLabel(account.account_type)}
          </Badge>
        )}
        {account.proxy_name && (
          <Badge color="grape" variant="light" size="sm">
            代理: {account.proxy_name}
          </Badge>
        )}
      </Group>

      {account.email && (
        <Text size="xs" c="dimmed" mb="xs" lineClamp={1}>{account.email}</Text>
      )}

      {probeCheckedAt && (
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
        (!account.health || account.health.state === "invalid") && (
          <Text size="xs" c="red" mb="xs">{account.invalid_reason}</Text>
        )}

      {effectiveProbeError && (
        <Text size="xs" c="orange" mb="xs">探测错误: {effectiveProbeError}</Text>
      )}

      {account.last_error &&
        (!account.health || account.health.state === "invalid") && (
          <Text size="xs" c="orange" mb="xs">OAuth: {account.last_error}</Text>
        )}

      <Divider my="xs" />

      <Stack gap="xs">
        <WindowRow label="5h 会话" window={rt?.session} />
        <WindowRow label="7d 总量" window={rt?.weekly} />
        <WindowRow label="7d Sonnet" window={rt?.weekly_sonnet} />
        <WindowRow label="7d Opus" window={rt?.weekly_opus} />
      </Stack>
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
  const [tab, setTab] = useState<"oauth" | "cookie">(editing?.auth_source === "cookie" ? "cookie" : "oauth");
  const [authUrl, setAuthUrl] = useState("");
  const [oauthState, setOauthState] = useState("");
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
    },
  });

  useEffect(() => {
    setTab(editing?.auth_source === "cookie" ? "cookie" : "oauth");
    setAuthUrl("");
    setOauthState("");
    form.setValues({
      name: editing?.name ?? "",
      rr_order: editing?.rr_order ?? 0,
      max_slots: 5,
      proxy_id: editing?.proxy_id ? String(editing.proxy_id) : null,
      drain_first: editing?.drain_first ?? false,
      cookie_blob: "",
      oauth_callback_input: "",
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
      if (!name) throw new ApiError(400, "名称必填");
      if (!editing && tab === "cookie" && !cookieBlob) throw new ApiError(400, "新账号必须提供 Cookie");
      if (!editing && tab === "oauth" && !oauthInput) throw new ApiError(400, "请粘贴 Callback URL 或 Code");

      if (editing) {
        const body: Record<string, unknown> = {};
        if (name !== editing.name) body.name = name;
        if (values.rr_order !== editing.rr_order) body.rr_order = values.rr_order;
        if ((editing.proxy_id ?? null) !== proxyId) body.proxy_id = proxyId ?? 0;
        if (values.drain_first !== editing.drain_first) body.drain_first = values.drain_first;
        if (cookieBlob) body.cookie_blob = cookieBlob;
        if (oauthInput) body.oauth_callback_input = oauthInput;
        if (scopedOauthState) body.oauth_state = scopedOauthState;
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
            onChange={(value) => {
              const nextTab = (value as "oauth" | "cookie") ?? "oauth";
              setTab(nextTab);
              if (nextTab === "cookie") {
                form.setFieldValue("oauth_callback_input", "");
                setAuthUrl("");
                setOauthState("");
              } else {
                form.setFieldValue("cookie_blob", "");
              }
            }}
          >
            <Tabs.List>
              <Tabs.Tab value="oauth">OAuth Token</Tabs.Tab>
              <Tabs.Tab value="cookie">Cookie</Tabs.Tab>
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
      const ids = query.state.data?.probing_ids ?? [];
      return ids.length > 0 ? 3000 : 30_000;
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
      queryClient.setQueryData(qk.accounts, (old: AccountsListResponse | undefined) =>
        old ? { ...old, probing_ids: resp.probing_ids } : old,
      );
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
  const probingIds = new Set(data?.probing_ids ?? []);
  const probeErrors = data?.probe_errors ?? {};

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
        <SimpleGrid cols={{ base: 1, md: 2, xl: 3 }}>
          {accounts.map((a) => (
            <AccountCard
              key={a.id}
              account={a}
              probing={probingIds.has(a.id)}
              probeError={probingIds.has(a.id) ? undefined : probeErrors[a.id]}
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
