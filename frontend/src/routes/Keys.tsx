import { useState } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import {
  Title,
  Table,
  Button,
  Group,
  Modal,
  Select,
  TextInput,
  Stack,
  Text,
  ActionIcon,
  Skeleton,
  Alert,
  ScrollArea,
  CopyButton,
  Code,
  Tooltip,
} from "@mantine/core";
import { notifications } from "@mantine/notifications";
import {
  IconPlus,
  IconTrash,
  IconCopy,
  IconCheck,
} from "@tabler/icons-react";
import {
  listKeys,
  listUsers,
  createKey,
  deleteKey,
  qk,
  ApiError,
  type KeyRow,
} from "../api";
import { formatDate } from "../lib/format";

function CreateKeyModal({
  opened,
  onClose,
}: {
  opened: boolean;
  onClose: () => void;
}) {
  const queryClient = useQueryClient();
  const { data: usersData } = useQuery({ queryKey: qk.users, queryFn: listUsers });
  const [userId, setUserId] = useState<string | null>(null);
  const [label, setLabel] = useState("");
  const [newKey, setNewKey] = useState<string | null>(null);

  const mutation = useMutation({
    mutationFn: () => createKey({ user_id: Number(userId), label: label || undefined }),
    onSuccess: (res) => {
      setNewKey(res.plaintext_key);
      queryClient.invalidateQueries({ queryKey: ["keys"] });
      queryClient.invalidateQueries({ queryKey: qk.overview });
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "创建失败", color: "red" }),
  });

  const handleClose = () => {
    setNewKey(null);
    setUserId(null);
    setLabel("");
    onClose();
  };

  const users = usersData?.items ?? [];
  const userOptions = users.map((u) => ({ value: String(u.id), label: `${u.username}${u.display_name ? ` (${u.display_name})` : ""}` }));

  return (
    <Modal opened={opened} onClose={handleClose} title="创建 API Key">
      <Stack>
        {newKey ? (
          <>
            <Alert color="green" title="Key 已生成 — 仅显示一次！">
              <Code block style={{ wordBreak: "break-all" }}>{newKey}</Code>
            </Alert>
            <CopyButton value={newKey}>
              {({ copied, copy }) => (
                <Button
                  fullWidth
                  color={copied ? "teal" : "blue"}
                  leftSection={copied ? <IconCheck size={16} /> : <IconCopy size={16} />}
                  onClick={copy}
                >
                  {copied ? "已复制" : "复制 Key"}
                </Button>
              )}
            </CopyButton>
          </>
        ) : (
          <>
            <Select
              label="用户"
              placeholder="选择用户"
              data={userOptions}
              value={userId}
              onChange={setUserId}
              required
              searchable
            />
            <TextInput
              label="标签（可选）"
              placeholder="如 claude-code-laptop"
              value={label}
              onChange={(e) => setLabel(e.currentTarget.value)}
            />
            <Group justify="flex-end">
              <Button variant="default" onClick={handleClose}>取消</Button>
              <Button
                onClick={() => mutation.mutate()}
                loading={mutation.isPending}
                disabled={!userId}
              >
                生成
              </Button>
            </Group>
          </>
        )}
      </Stack>
    </Modal>
  );
}

export default function Keys() {
  const queryClient = useQueryClient();
  const [createOpened, setCreateOpened] = useState(false);
  const [deleting, setDeleting] = useState<KeyRow | null>(null);

  const { data, isLoading, error } = useQuery({
    queryKey: ["keys"],
    queryFn: () => listKeys(),
  });

  const deleteMut = useMutation({
    mutationFn: () => deleteKey(deleting!.id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["keys"] });
      queryClient.invalidateQueries({ queryKey: qk.overview });
      notifications.show({ message: "Key 已删除", color: "green" });
      setDeleting(null);
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "删除失败", color: "red" }),
  });

  const keys: KeyRow[] = data?.items ?? [];

  return (
    <>
      <Group justify="space-between" mb="md">
        <Title order={3}>API 密钥</Title>
        <Button leftSection={<IconPlus size={16} />} onClick={() => setCreateOpened(true)}>
          创建密钥
        </Button>
      </Group>

      {isLoading ? (
        <Skeleton height={300} />
      ) : error ? (
        <Alert color="red">加载失败: {String(error)}</Alert>
      ) : keys.length === 0 ? (
        <Alert variant="light" color="blue">
          暂无 API 密钥。点击"创建密钥"为用户生成 Key，用于配置 Claude Code CLI。
        </Alert>
      ) : (
        <ScrollArea>
          <Table striped highlightOnHover verticalSpacing="sm">
            <Table.Thead>
              <Table.Tr>
                <Table.Th>Key</Table.Th>
                <Table.Th>用户</Table.Th>
                <Table.Th>标签</Table.Th>
                <Table.Th visibleFrom="md">最后使用</Table.Th>
              </Table.Tr>
            </Table.Thead>
            <Table.Tbody>
              {keys.map((k) => (
                <Table.Tr key={k.id}>
                  <Table.Td>
                    <Group gap={4} wrap="nowrap">
                      {k.plaintext_key ? (
                        <CopyButton value={k.plaintext_key}>
                          {({ copied, copy }) => (
                            <Tooltip label={copied ? "已复制" : "点击复制完整 Key"}>
                              <Code style={{ cursor: "pointer" }} onClick={copy}>
                                {copied ? "已复制!" : `sk-${k.lookup_key}...`}
                              </Code>
                            </Tooltip>
                          )}
                        </CopyButton>
                      ) : (
                        <Code>sk-{k.lookup_key}...</Code>
                      )}
                      <Tooltip label="删除">
                        <ActionIcon variant="subtle" color="red" size="sm" onClick={() => setDeleting(k)}>
                          <IconTrash size={14} />
                        </ActionIcon>
                      </Tooltip>
                    </Group>
                  </Table.Td>
                  <Table.Td>{k.username}</Table.Td>
                  <Table.Td>{k.label ?? "—"}</Table.Td>
                  <Table.Td visibleFrom="md">{formatDate(k.last_used_at)}</Table.Td>
                </Table.Tr>
              ))}
            </Table.Tbody>
          </Table>
        </ScrollArea>
      )}

      <CreateKeyModal opened={createOpened} onClose={() => setCreateOpened(false)} />

      <Modal opened={!!deleting} onClose={() => setDeleting(null)} title="删除密钥">
        <Stack>
          <Text>确定要删除 <Code>sk-{deleting?.lookup_key}...</Code> 吗？使用此 Key 的客户端将立即失去访问权限。</Text>
          <Group justify="flex-end">
            <Button variant="default" onClick={() => setDeleting(null)}>取消</Button>
            <Button color="red" loading={deleteMut.isPending} onClick={() => deleteMut.mutate()}>删除</Button>
          </Group>
        </Stack>
      </Modal>
    </>
  );
}
