import { useEffect, useState } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import {
  Title,
  Badge,
  Button,
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
  Divider,
  Tooltip,
  Tabs,
} from "@mantine/core";
import { useForm } from "@mantine/form";
import { notifications } from "@mantine/notifications";
import { IconPlus, IconEdit, IconTrash, IconRefresh, IconLink } from "@tabler/icons-react";
import {
  listAccounts,
  createAccount,
  updateAccount,
  deleteAccount,
  probeAllAccounts,
  startAccountOAuth,
  qk,
  ApiError,
  type Account,
  type AccountsListResponse,
  type UsageWindow,
} from "../api";
import { statusColor } from "../lib/format";

function accountTypeColor(t: string): string {
  switch (t) {
    case "max": return "violet";
    case "enterprise": return "indigo";
    case "pro": return "blue";
    case "free": return "gray";
    default: return "gray";
  }
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
  return (
    <Paper withBorder shadow="xs" radius="md" p="md">
      <Group justify="space-between" mb="xs">
        <Text fw={600}>{account.name}</Text>
        <Group gap={4}>
          <ActionIcon variant="subtle" size="sm" onClick={onEdit}>
            <IconEdit size={14} />
          </ActionIcon>
          <ActionIcon variant="subtle" size="sm" color="red" onClick={onDelete}>
            <IconTrash size={14} />
          </ActionIcon>
        </Group>
      </Group>

      <Group gap="xs" mb="xs">
        <Badge color={statusColor(account.status)} variant="light" size="sm">{account.status}</Badge>
        {probing && <Badge color="blue" variant="light" size="sm">探测中</Badge>}
        <Badge color="dark" variant="outline" size="sm">{account.auth_source}</Badge>
        {account.account_type && (
          <Badge color={accountTypeColor(account.account_type)} variant="light" size="sm">
            {account.account_type}
          </Badge>
        )}
      </Group>

      {account.email && (
        <Text size="xs" c="dimmed" mb="xs" lineClamp={1}>{account.email}</Text>
      )}

      {account.invalid_reason && (
        <Text size="xs" c="red" mb="xs">{account.invalid_reason}</Text>
      )}

      {probeError && (
        <Text size="xs" c="orange" mb="xs">探测错误: {probeError}</Text>
      )}

      {account.last_error && (
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
  cookie_blob: string;
  oauth_callback_input: string;
}

function AccountFormModal({
  opened,
  onClose,
  editing,
}: {
  opened: boolean;
  onClose: () => void;
  editing: Account | null;
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
      const cookieBlob = values.cookie_blob.trim();
      const oauthInput = values.oauth_callback_input.trim();
      if (!name) throw new ApiError(400, "名称必填");
      if (!editing && tab === "cookie" && !cookieBlob) throw new ApiError(400, "新账号必须提供 Cookie");
      if (!editing && tab === "oauth" && !oauthInput) throw new ApiError(400, "请粘贴 Callback URL 或 Code");

      if (editing) {
        const body: Record<string, unknown> = {};
        if (name !== editing.name) body.name = name;
        if (values.rr_order !== editing.rr_order) body.rr_order = values.rr_order;
        if (cookieBlob) body.cookie_blob = cookieBlob;
        if (oauthInput) body.oauth_callback_input = oauthInput;
        if (oauthState) body.oauth_state = oauthState;
        return updateAccount(editing.id, body);
      }
      return createAccount({
        name,
        max_slots: values.max_slots,
        auth_source: tab,
        cookie_blob: cookieBlob || undefined,
        oauth_callback_input: oauthInput || undefined,
        oauth_state: oauthState || undefined,
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
          <Tabs value={tab} onChange={(value) => setTab((value as "oauth" | "cookie") ?? "oauth")}>
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
              probeError={probeErrors[a.id]}
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
      />
      <DeleteModal account={deleting} onClose={() => setDeleting(null)} />
    </>
  );
}
