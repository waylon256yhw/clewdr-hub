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
} from "@mantine/core";
import { useForm } from "@mantine/form";
import { notifications } from "@mantine/notifications";
import { IconRefresh } from "@tabler/icons-react";
import { getSettings, updateSettings, changePassword, getCliVersions, qk, ApiError } from "../api";
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
        <ProxySection currentProxy={data["proxy"] ?? ""} />
        <PasswordSection />
      </Stack>
    </>
  );
}
