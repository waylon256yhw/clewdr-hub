import { useState } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import {
  Title,
  Paper,
  TextInput,
  PasswordInput,
  Button,
  Stack,
  Group,
  Text,
  Skeleton,
  Alert,
  Select,
  ActionIcon,
  Tooltip,
  Table,
  Switch,
  Badge,
  Modal,
  NumberInput,
  Popover,
} from "@mantine/core";
import { useForm } from "@mantine/form";
import { useDisclosure } from "@mantine/hooks";
import { notifications } from "@mantine/notifications";
import { IconRefresh, IconPlus, IconTrash } from "@tabler/icons-react";
import {
  getSettings, updateSettings, changePassword, getCliVersions,
  listModelsAdmin, createModel, updateModel, deleteModel, resetDefaultModels,
  qk, ApiError,
  type ModelRow,
} from "../api";
import { formatDate } from "../lib/format";

function VersionSection({ currentVersion }: { currentVersion: string }) {
  const queryClient = useQueryClient();
  const [refreshing, setRefreshing] = useState(false);
  const { data: versionsData, isLoading: versionsLoading } = useQuery({
    queryKey: ["cli-versions"],
    queryFn: () => getCliVersions(),
    staleTime: 3600_000,
  });

  const versions = versionsData?.versions ?? [];
  const fetchedAt = versionsData?.fetched_at ?? null;
  const selectData = versions.map((v) => ({ value: v, label: v }));

  if (currentVersion && !versions.includes(currentVersion)) {
    selectData.unshift({ value: currentVersion, label: `${currentVersion} (当前)` });
  }

  const mutation = useMutation({
    mutationFn: (version: string) => updateSettings({ cc_cli_version: version }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: qk.settings });
      queryClient.invalidateQueries({ queryKey: qk.overview });
      notifications.show({ message: "版本已更新", color: "green" });
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "保存失败", color: "red" }),
  });

  const handleRefresh = async () => {
    setRefreshing(true);
    try {
      const fresh = await getCliVersions(true);
      queryClient.setQueryData(["cli-versions"], fresh);
      notifications.show({ message: "版本列表已刷新", color: "green" });
    } catch {
      notifications.show({ message: "刷新失败", color: "red" });
    } finally {
      setRefreshing(false);
    }
  };

  return (
    <Paper shadow="xs" p="md" radius="md" withBorder>
      <Stack>
        <Group justify="space-between">
          <Text fw={600}>Claude Code 版本</Text>
          <Group gap="xs">
            {fetchedAt && (
              <Text size="xs" c="dimmed">
                {formatDate(fetchedAt)} 更新
              </Text>
            )}
            <Tooltip label="刷新版本列表">
              <ActionIcon
                variant="subtle"
                size="sm"
                onClick={handleRefresh}
                loading={refreshing}
              >
                <IconRefresh size={14} />
              </ActionIcon>
            </Tooltip>
          </Group>
        </Group>
        <Text size="sm" c="dimmed">
          选择要伪装的 Claude Code CLI 版本号。建议跟随官方最新版本。
        </Text>
        <Select
          label="CLI 版本"
          data={selectData}
          value={currentVersion}
          onChange={(v) => v && mutation.mutate(v)}
          disabled={versionsLoading || mutation.isPending}
          placeholder={versionsLoading ? "加载中..." : "选择版本"}
        />
      </Stack>
    </Paper>
  );
}

function ProxySection({ currentProxy }: { currentProxy: string }) {
  const queryClient = useQueryClient();
  const form = useForm({
    mode: "uncontrolled",
    initialValues: { proxy: currentProxy },
  });

  const mutation = useMutation({
    mutationFn: (values: { proxy: string }) => updateSettings({ proxy: values.proxy }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: qk.settings });
      notifications.show({ message: "代理设置已保存", color: "green" });
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "保存失败", color: "red" }),
  });

  return (
    <Paper shadow="xs" p="md" radius="md" withBorder>
      <form onSubmit={form.onSubmit((v) => mutation.mutate(v))}>
        <Stack>
          <Text fw={600}>代理设置</Text>
          <TextInput
            label="代理地址"
            placeholder="socks5://127.0.0.1:1080"
            {...form.getInputProps("proxy")}
          />
          <Group justify="flex-end">
            <Button type="submit" loading={mutation.isPending}>保存</Button>
          </Group>
        </Stack>
      </form>
    </Paper>
  );
}

function PasswordSection() {
  const form = useForm({
    mode: "uncontrolled",
    initialValues: { current_password: "", new_password: "", confirm: "" },
    validate: {
      new_password: (v) => (v.length < 6 ? "密码至少 6 个字符" : null),
      confirm: (v, values) => (v !== values.new_password ? "两次输入不一致" : null),
    },
  });

  const mutation = useMutation({
    mutationFn: (values: { current_password: string; new_password: string }) =>
      changePassword(values),
    onSuccess: () => {
      form.reset();
      notifications.show({ message: "密码已修改", color: "green" });
    },
    onError: (e) =>
      notifications.show({
        message: e instanceof ApiError ? e.message : "修改失败",
        color: "red",
      }),
  });

  return (
    <Paper shadow="xs" p="md" radius="md" withBorder>
      <form
        onSubmit={form.onSubmit(({ current_password, new_password }) =>
          mutation.mutate({ current_password, new_password }),
        )}
      >
        <Stack>
          <Text fw={600}>修改密码</Text>
          <PasswordInput
            label="当前密码"
            key={form.key("current_password")}
            {...form.getInputProps("current_password")}
          />
          <PasswordInput
            label="新密码"
            key={form.key("new_password")}
            {...form.getInputProps("new_password")}
          />
          <PasswordInput
            label="确认新密码"
            key={form.key("confirm")}
            {...form.getInputProps("confirm")}
          />
          <Group justify="flex-end">
            <Button type="submit" loading={mutation.isPending}>修改密码</Button>
          </Group>
        </Stack>
      </form>
    </Paper>
  );
}

function ModelsSection() {
  const queryClient = useQueryClient();
  const { data, isLoading, error: modelsError } = useQuery({ queryKey: qk.models, queryFn: listModelsAdmin });
  const [addOpened, { open: openAdd, close: closeAdd }] = useDisclosure(false);
  const [resetOpened, setResetOpened] = useState(false);

  const addForm = useForm({
    mode: "uncontrolled",
    initialValues: { model_id: "", display_name: "", sort_order: 0 },
    validate: {
      model_id: (v) => (v.trim() ? null : "必填"),
      display_name: (v) => (v.trim() ? null : "必填"),
    },
  });

  const addMutation = useMutation({
    mutationFn: (values: { model_id: string; display_name: string; sort_order: number }) =>
      createModel(values),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: qk.models });
      closeAdd();
      addForm.reset();
      notifications.show({ message: "模型已添加", color: "green" });
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "添加失败", color: "red" }),
  });

  const toggleMutation = useMutation({
    mutationFn: ({ modelId, enabled }: { modelId: string; enabled: boolean }) =>
      updateModel(modelId, { enabled }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: qk.models }),
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "更新失败", color: "red" }),
  });

  const deleteMutation = useMutation({
    mutationFn: (modelId: string) => deleteModel(modelId),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: qk.models });
      notifications.show({ message: "模型已删除", color: "green" });
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "删除失败", color: "red" }),
  });

  const resetMutation = useMutation({
    mutationFn: () => resetDefaultModels(),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: qk.models });
      setResetOpened(false);
      notifications.show({ message: "已重置为默认模型列表", color: "green" });
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "重置失败", color: "red" }),
  });

  const models: ModelRow[] = data?.items ?? [];

  const sourceBadge = (source: string) => {
    const color = source === "builtin" ? "blue" : source === "admin" ? "green" : "orange";
    return <Badge size="xs" variant="light" color={color}>{source}</Badge>;
  };

  return (
    <Paper shadow="xs" p="md" radius="md" withBorder>
      <Stack>
        <Group justify="space-between">
          <Text fw={600}>模型列表</Text>
          <Group gap="xs">
            <Popover opened={resetOpened} onChange={setResetOpened} withArrow>
              <Popover.Target>
                <Button variant="subtle" size="xs" onClick={() => setResetOpened(true)}>重置默认</Button>
              </Popover.Target>
              <Popover.Dropdown>
                <Stack gap="xs">
                  <Text size="sm">确认重置？将清除所有自定义模型。</Text>
                  <Group justify="flex-end" gap="xs">
                    <Button size="xs" variant="default" onClick={() => setResetOpened(false)}>取消</Button>
                    <Button size="xs" color="red" loading={resetMutation.isPending} onClick={() => resetMutation.mutate()}>确认重置</Button>
                  </Group>
                </Stack>
              </Popover.Dropdown>
            </Popover>
            <Button size="xs" leftSection={<IconPlus size={14} />} onClick={openAdd}>添加模型</Button>
          </Group>
        </Group>
        <Text size="sm" c="dimmed">
          管理对外公布的模型列表。禁用的模型不会出现在 /v1/models 接口中，但仍可通过 API 调用。
        </Text>
        {isLoading ? (
          <Skeleton height={200} />
        ) : modelsError ? (
          <Alert color="red" title="加载失败">{String(modelsError)}</Alert>
        ) : (
          <Table striped highlightOnHover>
            <Table.Thead>
              <Table.Tr>
                <Table.Th>模型 ID</Table.Th>
                <Table.Th>显示名</Table.Th>
                <Table.Th>来源</Table.Th>
                <Table.Th>排序</Table.Th>
                <Table.Th>启用</Table.Th>
                <Table.Th />
              </Table.Tr>
            </Table.Thead>
            <Table.Tbody>
              {models.map((m) => (
                <Table.Tr key={m.model_id}>
                  <Table.Td><Text size="sm" ff="monospace">{m.model_id}</Text></Table.Td>
                  <Table.Td><Text size="sm">{m.display_name}</Text></Table.Td>
                  <Table.Td>{sourceBadge(m.source)}</Table.Td>
                  <Table.Td><Text size="sm">{m.sort_order}</Text></Table.Td>
                  <Table.Td>
                    <Switch
                      checked={m.enabled === 1}
                      onChange={(e) => toggleMutation.mutate({ modelId: m.model_id, enabled: e.currentTarget.checked })}
                    />
                  </Table.Td>
                  <Table.Td>
                    <Tooltip label="删除">
                      <ActionIcon variant="subtle" color="red" size="sm" onClick={() => deleteMutation.mutate(m.model_id)}>
                        <IconTrash size={14} />
                      </ActionIcon>
                    </Tooltip>
                  </Table.Td>
                </Table.Tr>
              ))}
              {models.length === 0 && (
                <Table.Tr>
                  <Table.Td colSpan={6}><Text size="sm" c="dimmed" ta="center">暂无模型</Text></Table.Td>
                </Table.Tr>
              )}
            </Table.Tbody>
          </Table>
        )}
      </Stack>

      <Modal opened={addOpened} onClose={closeAdd} title="添加模型">
        <form onSubmit={addForm.onSubmit((v) => addMutation.mutate(v))}>
          <Stack>
            <TextInput label="模型 ID" placeholder="claude-xxx-x-x" key={addForm.key("model_id")} {...addForm.getInputProps("model_id")} />
            <TextInput label="显示名" placeholder="Claude XXX X.X" key={addForm.key("display_name")} {...addForm.getInputProps("display_name")} />
            <NumberInput label="排序" key={addForm.key("sort_order")} {...addForm.getInputProps("sort_order")} />
            <Group justify="flex-end">
              <Button type="submit" loading={addMutation.isPending}>添加</Button>
            </Group>
          </Stack>
        </form>
      </Modal>
    </Paper>
  );
}

export default function Settings() {
  const { data, isLoading, error } = useQuery({
    queryKey: qk.settings,
    queryFn: getSettings,
  });

  if (isLoading) return <Skeleton height={400} radius="md" />;
  if (error || !data) {
    return (
      <Alert color="red" title="加载失败">
        {String(error ?? "未知错误")}
      </Alert>
    );
  }

  return (
    <>
      <Title order={3} mb="md">设置</Title>
      <Stack>
        <VersionSection currentVersion={data["cc_cli_version"] ?? ""} />
        <ModelsSection />
        <ProxySection currentProxy={data["proxy"] ?? ""} />
        <PasswordSection />
      </Stack>
    </>
  );
}
