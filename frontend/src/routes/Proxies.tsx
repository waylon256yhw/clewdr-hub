import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ActionIcon,
  Alert,
  Badge,
  Button,
  Group,
  Modal,
  Paper,
  PasswordInput,
  Skeleton,
  SimpleGrid,
  Stack,
  Text,
  TextInput,
  Title,
} from "@mantine/core";
import { useForm } from "@mantine/form";
import { notifications } from "@mantine/notifications";
import { IconEdit, IconFlask, IconPlus, IconTrash } from "@tabler/icons-react";
import {
  ApiError,
  createProxy,
  deleteProxy,
  listProxies,
  qk,
  testProxy,
  type Proxy,
  updateProxy,
} from "../api";
import { formatDate } from "../lib/format";

interface ProxyFormValues {
  name: string;
  address: string;
  username: string;
  password: string;
}

function getProxyFormValues(editing: Proxy | null): ProxyFormValues {
  return {
    name: editing?.name ?? "",
    address: editing ? buildAddress(editing) : "",
    username: editing?.username ?? "",
    password: editing?.password ?? "",
  };
}

function buildAddress(proxy: Proxy): string {
  return `${proxy.protocol}://${proxy.host}:${proxy.port}`;
}

function parseProxyAddress(address: string): {
  protocol: Proxy["protocol"];
  host: string;
  port: number;
} {
  const trimmed = address.trim();
  let url: URL;
  try {
    url = new URL(trimmed);
  } catch {
    throw new ApiError(400, "代理地址格式无效");
  }

  const protocol = url.protocol.replace(/:$/, "") as Proxy["protocol"];
  if (!["http", "https", "socks5", "socks5h"].includes(protocol)) {
    throw new ApiError(400, "仅支持 http/https/socks5/socks5h");
  }
  if (!url.hostname || !url.port) {
    throw new ApiError(400, "代理地址必须包含主机和端口");
  }

  return {
    protocol,
    host: url.hostname,
    port: Number(url.port),
  };
}

function latencyColor(latencyMs: number | null): string | undefined {
  if (latencyMs === null) return undefined;
  if (latencyMs < 300) return "var(--mantine-color-green-7)";
  if (latencyMs <= 800) return "var(--mantine-color-yellow-7)";
  return "var(--mantine-color-red-7)";
}

function normalizeCountryLabel(country: string | null): string | null {
  if (!country) return null;
  const normalized = country
    .replace(/^[\u{1F1E6}-\u{1F1FF}]{2}\s*/u, "")
    .trim();
  return normalized || null;
}

function ProxyFormModal({
  opened,
  onClose,
  editing,
}: {
  opened: boolean;
  onClose: () => void;
  editing: Proxy | null;
}) {
  const queryClient = useQueryClient();
  const form = useForm<ProxyFormValues>({
    mode: "uncontrolled",
    initialValues: getProxyFormValues(editing),
  });

  useEffect(() => {
    if (!opened) return;
    form.setValues(getProxyFormValues(editing));
  }, [editing, opened]); // eslint-disable-line react-hooks/exhaustive-deps

  const mutation = useMutation({
    mutationFn: async (values: ProxyFormValues) => {
      const parsed = parseProxyAddress(values.address);
      const username = values.username.trim();
      const password = values.password.trim();
      const payload = {
        name: values.name.trim(),
        protocol: parsed.protocol,
        host: parsed.host,
        port: parsed.port,
        username: editing ? username : (username || undefined),
        password: editing ? password : (password || undefined),
      };
      if (!payload.name) throw new ApiError(400, "名称必填");
      if (editing) return updateProxy(editing.id, payload);
      return createProxy(payload);
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: qk.proxies });
      queryClient.invalidateQueries({ queryKey: qk.accounts });
      notifications.show({ message: editing ? "代理已更新" : "代理已创建", color: "green" });
      onClose();
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "保存失败", color: "red" }),
  });

  return (
    <Modal opened={opened} onClose={onClose} title={editing ? "编辑代理" : "添加代理"}>
      <form onSubmit={form.onSubmit((values) => mutation.mutate(values))}>
        <Stack>
          <TextInput label="名称" required key={form.key("name")} {...form.getInputProps("name")} />
          <TextInput
            label="代理地址"
            description="例如 socks5://127.0.0.1:1080"
            required
            key={form.key("address")}
            {...form.getInputProps("address")}
          />
          <TextInput label="用户名（可选）" key={form.key("username")} {...form.getInputProps("username")} />
          <PasswordInput label="密码（可选）" key={form.key("password")} {...form.getInputProps("password")} />
          <Group justify="flex-end">
            <Button variant="default" onClick={onClose}>取消</Button>
            <Button type="submit" loading={mutation.isPending}>{editing ? "保存" : "创建"}</Button>
          </Group>
        </Stack>
      </form>
    </Modal>
  );
}

function DeleteModal({
  proxy,
  onClose,
}: {
  proxy: Proxy | null;
  onClose: () => void;
}) {
  const queryClient = useQueryClient();
  const mutation = useMutation({
    mutationFn: () => deleteProxy(proxy!.id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: qk.proxies });
      queryClient.invalidateQueries({ queryKey: qk.accounts });
      notifications.show({ message: "代理已删除", color: "green" });
      onClose();
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "删除失败", color: "red" }),
  });

  return (
    <Modal opened={!!proxy} onClose={onClose} title="删除代理">
      <Stack>
        <Text>确定删除代理 <strong>{proxy?.name}</strong>？</Text>
        <Text size="sm" c="dimmed">仅删除代理记录，不代表这个地址不可用。</Text>
        <Group justify="flex-end">
          <Button variant="default" onClick={onClose}>取消</Button>
          <Button color="red" loading={mutation.isPending} onClick={() => mutation.mutate()}>删除</Button>
        </Group>
      </Stack>
    </Modal>
  );
}

function ProxyCard({
  proxy,
  onEdit,
  onDelete,
}: {
  proxy: Proxy;
  onEdit: () => void;
  onDelete: () => void;
}) {
  const queryClient = useQueryClient();
  const testMut = useMutation({
    mutationFn: () => testProxy(proxy.id),
    onSuccess: (result) => {
      queryClient.invalidateQueries({ queryKey: qk.proxies });
      if (result.success) {
        notifications.show({
          message: `测试通过${result.latency_ms ? ` (${result.latency_ms}ms)` : ""}`,
          color: "green",
        });
      } else {
        notifications.show({ message: result.message || "测试失败", color: "red" });
      }
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "测试失败", color: "red" }),
  });

  const location = useMemo(() => {
    return [
      normalizeCountryLabel(proxy.last_test_country),
      proxy.last_test_region,
      proxy.last_test_city,
    ]
      .filter(Boolean)
      .join(" / ");
  }, [proxy.last_test_city, proxy.last_test_country, proxy.last_test_region]);

  const metricCards = [
    {
      label: "延迟",
      value:
        proxy.last_test_latency_ms !== null
          ? `${proxy.last_test_latency_ms} ms`
          : proxy.last_test_success === false
            ? "测试失败"
            : "未测试",
      color: proxy.last_test_success === false ? "var(--mantine-color-red-7)" : latencyColor(proxy.last_test_latency_ms),
    },
    {
      label: "出口 IP",
      value: proxy.last_test_ip_address ?? "未获取",
      color: undefined,
    },
    {
      label: "出口位置",
      value: location || "未获取",
      color: undefined,
    },
  ];

  return (
    <Paper withBorder shadow="xs" radius="md" p="md">
      <Group justify="space-between" mb="xs">
        <Text fw={600}>{proxy.name}</Text>
        <Group gap={4}>
          <ActionIcon variant="subtle" color="cyan" loading={testMut.isPending} onClick={() => testMut.mutate()}>
            <IconFlask size={14} />
          </ActionIcon>
          <ActionIcon variant="subtle" onClick={onEdit}>
            <IconEdit size={14} />
          </ActionIcon>
          <ActionIcon variant="subtle" color="red" onClick={onDelete}>
            <IconTrash size={14} />
          </ActionIcon>
        </Group>
      </Group>

      <Stack gap="xs">
        <Group gap="xs">
          <Badge variant="light">{proxy.protocol}</Badge>
          {proxy.username && <Badge variant="outline">鉴权</Badge>}
          {proxy.last_test_success !== null && (
            <Badge color={proxy.last_test_success ? "green" : "red"} variant="light">
              {proxy.last_test_success ? "可用" : "失败"}
            </Badge>
          )}
        </Group>
        <Text size="sm" ff="monospace">{buildAddress(proxy)}</Text>
        <SimpleGrid cols={{ base: 1, sm: 3 }} spacing="sm">
          {metricCards.map((item) => (
            <Paper key={item.label} withBorder radius="md" p="sm">
              <Text size="xs" tt="uppercase" fw={700} c="dimmed">{item.label}</Text>
              <Text
                mt={4}
                fw={700}
                size={item.label === "延迟" ? "lg" : "sm"}
                ff={item.label === "出口 IP" ? "monospace" : undefined}
                lineClamp={2}
                c={item.color}
              >
                {item.value}
              </Text>
            </Paper>
          ))}
        </SimpleGrid>
        {proxy.last_test_success === false && proxy.last_test_message && (
          <Text size="xs" c="red">
            {proxy.last_test_message}
          </Text>
        )}
        {proxy.last_test_at && (
          <Text size="xs" c="dimmed">最后测试: {formatDate(proxy.last_test_at)}</Text>
        )}
      </Stack>
    </Paper>
  );
}

export default function Proxies() {
  const { data, isLoading, error } = useQuery({
    queryKey: qk.proxies,
    queryFn: listProxies,
  });
  const [formOpened, setFormOpened] = useState(false);
  const [editing, setEditing] = useState<Proxy | null>(null);
  const [deleting, setDeleting] = useState<Proxy | null>(null);

  if (isLoading) return <Skeleton height={320} radius="md" />;
  if (error) {
    return (
      <Alert color="red" title="加载失败">
        {String(error)}
      </Alert>
    );
  }

  const proxies = data?.items ?? [];

  return (
    <>
      <Group justify="space-between" mb="md">
        <div>
          <Title order={3}>代理</Title>
          <Text size="sm" c="dimmed">仅测试基础连通性，不代表一定适用于特定上游服务。</Text>
        </div>
        <Button leftSection={<IconPlus size={16} />} onClick={() => {
          setEditing(null);
          setFormOpened(true);
        }}>
          添加代理
        </Button>
      </Group>

      {proxies.length === 0 ? (
        <Text c="dimmed">暂无代理，点击右上角添加。</Text>
      ) : (
        <Stack>
          {proxies.map((proxy) => (
            <ProxyCard
              key={proxy.id}
              proxy={proxy}
              onEdit={() => {
                setEditing(proxy);
                setFormOpened(true);
              }}
              onDelete={() => setDeleting(proxy)}
            />
          ))}
        </Stack>
      )}

      <ProxyFormModal
        key={editing?.id ?? "new"}
        opened={formOpened}
        onClose={() => {
          setFormOpened(false);
          setEditing(null);
        }}
        editing={editing}
      />
      <DeleteModal proxy={deleting} onClose={() => setDeleting(null)} />
    </>
  );
}
