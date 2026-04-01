import { useState } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import {
  Title,
  Table,
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
  Tooltip,
  ScrollArea,
} from "@mantine/core";
import { useForm } from "@mantine/form";
import { notifications } from "@mantine/notifications";
import { IconPlus, IconEdit, IconTrash } from "@tabler/icons-react";
import {
  listAccounts,
  createAccount,
  updateAccount,
  deleteAccount,
  qk,
  ApiError,
  type Account,
} from "../api";
import { formatDate, statusColor } from "../lib/format";

interface FormValues {
  name: string;
  rr_order: number;
  max_slots: number;
  cookie_blob: string;
  organization_uuid: string;
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
  const form = useForm<FormValues>({
    mode: "uncontrolled",
    initialValues: {
      name: editing?.name ?? "",
      rr_order: editing?.rr_order ?? 0,
      max_slots: editing?.max_slots ?? 5,
      cookie_blob: "",
      organization_uuid: editing?.organization_uuid ?? "",
    },
    validate: {
      name: (v) => (v.trim() ? null : "必填"),
      max_slots: (v) => (v > 0 ? null : "必须大于 0"),
      cookie_blob: (v) => (!editing && !v.trim() ? "新账号必须提供 Cookie" : null),
    },
  });

  const mutation = useMutation({
    mutationFn: async (values: FormValues) => {
      if (editing) {
        const body: Record<string, unknown> = {};
        if (values.name !== editing.name) body.name = values.name;
        if (values.rr_order !== editing.rr_order) body.rr_order = values.rr_order;
        if (values.max_slots !== editing.max_slots) body.max_slots = values.max_slots;
        if (values.cookie_blob.trim()) body.cookie_blob = values.cookie_blob;
        if (values.organization_uuid !== (editing.organization_uuid ?? ""))
          body.organization_uuid = values.organization_uuid || null;
        return updateAccount(editing.id, body);
      }
      return createAccount({
        name: values.name,
        max_slots: values.max_slots,
        cookie_blob: values.cookie_blob,
        organization_uuid: values.organization_uuid || undefined,
      });
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: qk.accounts });
      queryClient.invalidateQueries({ queryKey: qk.overview });
      notifications.show({ message: editing ? "账号已更新" : "账号已创建", color: "green" });
      form.reset();
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
          <NumberInput label="最大并发" min={1} key={form.key("max_slots")} {...form.getInputProps("max_slots")} />
          <Textarea
            label={editing ? "替换 Cookie（可选）" : "Cookie"}
            placeholder="粘贴 Cookie..."
            autosize
            minRows={3}
            key={form.key("cookie_blob")}
            {...form.getInputProps("cookie_blob")}
          />
          <TextInput
            label="组织 UUID（可选）"
            key={form.key("organization_uuid")}
            {...form.getInputProps("organization_uuid")}
          />
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
  const { data, isLoading, error } = useQuery({
    queryKey: qk.accounts,
    queryFn: listAccounts,
  });
  const [formOpened, setFormOpened] = useState(false);
  const [editing, setEditing] = useState<Account | null>(null);
  const [deleting, setDeleting] = useState<Account | null>(null);

  if (isLoading) return <Skeleton height={300} radius="md" />;
  if (error) {
    return (
      <Alert color="red" title="Failed to load accounts">
        {String(error)}
      </Alert>
    );
  }

  const accounts = data?.items ?? [];

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
        <Button leftSection={<IconPlus size={16} />} onClick={openCreate}>
          添加账号
        </Button>
      </Group>

      {accounts.length === 0 ? (
        <Text c="dimmed">暂无账号，点击上方按钮添加。</Text>
      ) : (
        <ScrollArea>
          <Table striped highlightOnHover verticalSpacing="sm">
            <Table.Thead>
              <Table.Tr>
                <Table.Th>名称</Table.Th>
                <Table.Th>状态</Table.Th>
                <Table.Th>并发</Table.Th>
                <Table.Th visibleFrom="md">组织 UUID</Table.Th>
                <Table.Th>最后使用</Table.Th>
                <Table.Th visibleFrom="md">最近错误</Table.Th>
                <Table.Th w={100}>操作</Table.Th>
              </Table.Tr>
            </Table.Thead>
            <Table.Tbody>
              {accounts.map((a) => (
                <Table.Tr key={a.id}>
                  <Table.Td>{a.name}</Table.Td>
                  <Table.Td>
                    <Badge color={statusColor(a.status)} variant="light">{a.status}</Badge>
                  </Table.Td>
                  <Table.Td>{a.max_slots}</Table.Td>
                  <Table.Td visibleFrom="md">
                    <Text size="xs" lineClamp={1}>{a.organization_uuid ?? "—"}</Text>
                  </Table.Td>
                  <Table.Td>{formatDate(a.last_used_at)}</Table.Td>
                  <Table.Td visibleFrom="md">
                    <Tooltip label={a.last_error ?? ""} disabled={!a.last_error}>
                      <Text size="xs" lineClamp={1} c={a.last_error ? "red" : "dimmed"}>
                        {a.last_error ?? "—"}
                      </Text>
                    </Tooltip>
                  </Table.Td>
                  <Table.Td>
                    <Group gap={4}>
                      <ActionIcon variant="subtle" onClick={() => openEdit(a)}>
                        <IconEdit size={16} />
                      </ActionIcon>
                      <ActionIcon variant="subtle" color="red" onClick={() => setDeleting(a)}>
                        <IconTrash size={16} />
                      </ActionIcon>
                    </Group>
                  </Table.Td>
                </Table.Tr>
              ))}
            </Table.Tbody>
          </Table>
        </ScrollArea>
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
