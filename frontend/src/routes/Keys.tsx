import { useRef, useState } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import {
  Title,
  Table,
  Badge,
  Button,
  Group,
  Modal,
  MultiSelect,
  Select,
  TextInput,
  Stack,
  Text,
  ActionIcon,
  Skeleton,
  Alert,
  ScrollArea,
  Code,
  Tooltip,
  Textarea,
} from "@mantine/core";
import { notifications } from "@mantine/notifications";
import {
  IconPlus,
  IconTrash,
  IconCopy,
  IconCheck,
  IconLink,
  IconEye,
} from "@tabler/icons-react";
import {
  listKeys,
  listUsers,
  listAccounts,
  createKey,
  deleteKey,
  updateKeyBindings,
  qk,
  ApiError,
  type KeyRow,
} from "../api";
import { copyText, getCopyFailureMessage, selectTextField } from "../lib/clipboard";
import { formatDate } from "../lib/format";

function notifyCopyFailure(prefix: string, error: unknown) {
  notifications.show({
    message: `${prefix}${getCopyFailureMessage(error)}`,
    color: "yellow",
  });
}

function CreateKeyModal({
  opened,
  onClose,
}: {
  opened: boolean;
  onClose: () => void;
}) {
  const queryClient = useQueryClient();
  const { data: usersData } = useQuery({ queryKey: qk.users, queryFn: listUsers });
  const { data: accountsData } = useQuery({ queryKey: qk.accounts, queryFn: listAccounts });
  const [userId, setUserId] = useState<string | null>(null);
  const [label, setLabel] = useState("");
  const [boundIds, setBoundIds] = useState<string[]>([]);
  const [newKey, setNewKey] = useState<string | null>(null);
  const [newKeyCopied, setNewKeyCopied] = useState(false);
  const newKeyFieldRef = useRef<HTMLTextAreaElement | null>(null);

  const mutation = useMutation({
    mutationFn: () => createKey({
      user_id: Number(userId),
      label: label || undefined,
      bound_account_ids: boundIds.length > 0 ? boundIds.map(Number) : undefined,
    }),
    onSuccess: async (res) => {
      setNewKey(res.plaintext_key);
      setNewKeyCopied(false);
      queryClient.invalidateQueries({ queryKey: ["keys"] });
      queryClient.invalidateQueries({ queryKey: qk.overview });

      try {
        await copyText(res.plaintext_key);
        setNewKeyCopied(true);
        notifications.show({ message: "Key 已生成并复制", color: "green" });
      } catch (error) {
        window.setTimeout(() => selectTextField(newKeyFieldRef.current), 0);
        notifyCopyFailure("Key 已生成，但自动复制失败。", error);
      }
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "创建失败", color: "red" }),
  });

  const handleNewKeyCopy = async () => {
    if (!newKey) return;

    try {
      await copyText(newKey);
      setNewKeyCopied(true);
      notifications.show({ message: "Key 已复制", color: "green" });
    } catch (error) {
      setNewKeyCopied(false);
      selectTextField(newKeyFieldRef.current);
      notifyCopyFailure("复制失败，已自动选中文本。", error);
    }
  };

  const handleClose = () => {
    setNewKey(null);
    setNewKeyCopied(false);
    setUserId(null);
    setLabel("");
    setBoundIds([]);
    onClose();
  };

  const users = usersData?.items ?? [];
  const accounts = accountsData?.items ?? [];
  const userOptions = users.map((u) => ({ value: String(u.id), label: `${u.username}${u.display_name ? ` (${u.display_name})` : ""}` }));
  const accountOptions = accounts.map((a) => ({ value: String(a.id), label: a.name }));

  return (
    <Modal
      opened={opened}
      onClose={handleClose}
      title="创建 API Key"
      size={newKey ? "sm" : "md"}
      centered
    >
      <Stack>
        {newKey ? (
          <>
            <Alert
              color={newKeyCopied ? "green" : "yellow"}
              title={newKeyCopied ? "已自动复制" : "请复制并保存"}
            >
              <Textarea
                ref={newKeyFieldRef}
                value={newKey}
                readOnly
                minRows={2}
                maxRows={2}
                styles={{ input: { fontFamily: "monospace", wordBreak: "break-all" } }}
              />
            </Alert>
            <Group grow>
              <Button
                color={newKeyCopied ? "teal" : "blue"}
                leftSection={newKeyCopied ? <IconCheck size={16} /> : <IconCopy size={16} />}
                onClick={handleNewKeyCopy}
              >
                {newKeyCopied ? "已复制" : "复制 Key"}
              </Button>
              <Button variant="default" onClick={handleClose}>我已保存</Button>
            </Group>
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
            <MultiSelect
              label="绑定账号（可选）"
              placeholder="留空 = 使用全部账号"
              data={accountOptions}
              value={boundIds}
              onChange={setBoundIds}
              searchable
              clearable
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

function BindingsModal({
  keyItem,
  onClose,
}: {
  keyItem: KeyRow | null;
  onClose: () => void;
}) {
  const queryClient = useQueryClient();
  const { data: accountsData } = useQuery({ queryKey: qk.accounts, queryFn: listAccounts });
  const [selected, setSelected] = useState<string[]>(
    keyItem?.bound_account_ids.map(String) ?? [],
  );

  const mutation = useMutation({
    mutationFn: () => updateKeyBindings(keyItem!.id, selected.map(Number)),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["keys"] });
      notifications.show({ message: "绑定已更新", color: "green" });
      onClose();
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "更新失败", color: "red" }),
  });

  const accounts = accountsData?.items ?? [];
  const accountOptions = accounts.map((a) => ({ value: String(a.id), label: a.name }));

  return (
    <Modal opened={!!keyItem} onClose={onClose} title={`绑定账号 — ${keyItem?.label || `sk-${keyItem?.lookup_key}...`}`}>
      <Stack>
        <Text size="sm" c="dimmed">选择此 Key 可使用的上游账号。留空表示不限制。</Text>
        <MultiSelect
          label="绑定账号"
          placeholder="全部可用"
          data={accountOptions}
          value={selected}
          onChange={setSelected}
          searchable
          clearable
        />
        <Group justify="flex-end">
          <Button variant="default" onClick={onClose}>取消</Button>
          <Button onClick={() => mutation.mutate()} loading={mutation.isPending}>保存</Button>
        </Group>
      </Stack>
    </Modal>
  );
}

export default function Keys() {
  const queryClient = useQueryClient();
  const [createOpened, setCreateOpened] = useState(false);
  const [deleting, setDeleting] = useState<KeyRow | null>(null);
  const [binding, setBinding] = useState<KeyRow | null>(null);
  const [revealed, setRevealed] = useState<KeyRow | null>(null);
  const [revealedCopied, setRevealedCopied] = useState(false);
  const revealedFieldRef = useRef<HTMLTextAreaElement | null>(null);

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
  const insecureContext = typeof window !== "undefined" && !window.isSecureContext;

  const handleRevealCopy = async () => {
    if (!revealed?.plaintext_key) return;

    try {
      await copyText(revealed.plaintext_key);
      setRevealedCopied(true);
      notifications.show({ message: "Key 已复制", color: "green" });
    } catch (error) {
      setRevealedCopied(false);
      selectTextField(revealedFieldRef.current);
      notifyCopyFailure("复制失败，已自动选中文本。", error);
    }
  };

  const closeRevealModal = () => {
    setRevealed(null);
    setRevealedCopied(false);
  };

  return (
    <>
      <Group justify="space-between" mb="md">
        <Title order={3}>API 密钥</Title>
        <Button leftSection={<IconPlus size={16} />} onClick={() => setCreateOpened(true)}>
          创建密钥
        </Button>
      </Group>

      {insecureContext && (
        <Alert color="yellow" variant="light" mb="md">
          当前访问不是安全上下文，复制可能被浏览器拦截。
        </Alert>
      )}

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
          <Table striped highlightOnHover verticalSpacing="xs">
            <Table.Thead>
              <Table.Tr>
                <Table.Th>Key</Table.Th>
                <Table.Th>用户</Table.Th>
                <Table.Th visibleFrom="sm">绑定</Table.Th>
                <Table.Th visibleFrom="md">最后使用</Table.Th>
                <Table.Th style={{ width: 80 }}>操作</Table.Th>
              </Table.Tr>
            </Table.Thead>
            <Table.Tbody>
              {keys.map((k) => (
                <Table.Tr key={k.id}>
                  <Table.Td>
                    {k.plaintext_key ? (
                      <Code>{`sk-${k.lookup_key}...`}</Code>
                    ) : (
                      <Code>sk-{k.lookup_key}...</Code>
                    )}
                    {k.label && <Text size="xs" c="dimmed">{k.label}</Text>}
                  </Table.Td>
                  <Table.Td><Text size="sm">{k.username}</Text></Table.Td>
                  <Table.Td visibleFrom="sm">
                    {k.bound_account_ids.length === 0 ? (
                      <Text size="xs" c="dimmed">全部</Text>
                    ) : (
                      <Group gap={4}>
                        {k.bound_account_ids.map((id) => (
                          <Badge key={id} size="xs" variant="light">#{id}</Badge>
                        ))}
                      </Group>
                    )}
                  </Table.Td>
                  <Table.Td visibleFrom="md"><Text size="xs">{formatDate(k.last_used_at)}</Text></Table.Td>
                  <Table.Td>
                    <Group gap={4} wrap="nowrap">
                      <Tooltip label="绑定账号">
                        <ActionIcon variant="subtle" size="sm" onClick={() => setBinding(k)}>
                          <IconLink size={14} />
                        </ActionIcon>
                      </Tooltip>
                      {k.plaintext_key && (
                        <Tooltip label="查看/复制">
                          <ActionIcon
                            variant="subtle"
                            size="sm"
                            onClick={() => {
                              setRevealed(k);
                              setRevealedCopied(false);
                            }}
                          >
                            <IconEye size={14} />
                          </ActionIcon>
                        </Tooltip>
                      )}
                      <Tooltip label="删除">
                        <ActionIcon variant="subtle" color="red" size="sm" onClick={() => setDeleting(k)}>
                          <IconTrash size={14} />
                        </ActionIcon>
                      </Tooltip>
                    </Group>
                  </Table.Td>
                </Table.Tr>
              ))}
            </Table.Tbody>
          </Table>
        </ScrollArea>
      )}

      <CreateKeyModal opened={createOpened} onClose={() => setCreateOpened(false)} />
      <BindingsModal key={binding?.id ?? "none"} keyItem={binding} onClose={() => setBinding(null)} />

      <Modal
        opened={!!revealed}
        onClose={closeRevealModal}
        title={revealed?.label || "查看 / 复制"}
        size="sm"
        centered
      >
        <Stack>
          <Alert
            color={revealedCopied ? "green" : "yellow"}
            title={revealedCopied ? "已复制" : "完整 Key"}
          >
            <Textarea
              ref={revealedFieldRef}
              value={revealed?.plaintext_key ?? ""}
              readOnly
              minRows={2}
              maxRows={2}
              styles={{ input: { fontFamily: "monospace", wordBreak: "break-all" } }}
            />
          </Alert>
          <Group grow>
            <Button
              color={revealedCopied ? "teal" : "blue"}
              leftSection={revealedCopied ? <IconCheck size={16} /> : <IconCopy size={16} />}
              onClick={handleRevealCopy}
            >
              {revealedCopied ? "已复制" : "复制 Key"}
            </Button>
            <Button variant="default" onClick={closeRevealModal}>关闭</Button>
          </Group>
        </Stack>
      </Modal>

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
