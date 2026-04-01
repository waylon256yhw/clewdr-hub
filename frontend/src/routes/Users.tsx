import { useState } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import {
  Title,
  Tabs,
  Table,
  Badge,
  Button,
  Group,
  Modal,
  TextInput,
  PasswordInput,
  NumberInput,
  Select,
  Stack,
  Text,
  ActionIcon,
  Skeleton,
  Alert,
  ScrollArea,
  Tooltip,
} from "@mantine/core";
import { useForm } from "@mantine/form";
import { notifications } from "@mantine/notifications";
import {
  IconPlus,
  IconEdit,
  IconTrash,
  IconPlayerPlay,
  IconPlayerPause,
} from "@tabler/icons-react";
import {
  listUsers,
  createUser,
  updateUser,
  deleteUser,
  listPolicies,
  createPolicy,
  updatePolicy,
  deletePolicy,
  qk,
  ApiError,
  type UserRow,
  type Policy,
} from "../api";
import { formatCost, formatDate } from "../lib/format";

// ─── User Form Modal ───

function UserFormModal({
  opened,
  onClose,
  editing,
  policies,
}: {
  opened: boolean;
  onClose: () => void;
  editing: UserRow | null;
  policies: Policy[];
}) {
  const queryClient = useQueryClient();
  const form = useForm({
    mode: "uncontrolled",
    initialValues: {
      username: editing?.username ?? "",
      display_name: editing?.display_name ?? "",
      password: "",
      role: editing?.role ?? "member",
      policy_id: String(editing?.policy_id ?? policies[0]?.id ?? 1),
      notes: editing?.notes ?? "",
    },
    validate: {
      username: (v) => (v.trim() ? null : "必填"),
      password: (v, values) => {
        if (!editing && values.role === "admin" && !v) return "管理员必须设置密码";
        return null;
      },
    },
  });

  const mutation = useMutation({
    mutationFn: async (values: typeof form.values) => {
      const body: Record<string, unknown> = {};
      if (editing) {
        if (values.username !== editing.username) body.username = values.username;
        if (values.display_name !== (editing.display_name ?? ""))
          body.display_name = values.display_name || null;
        if (values.password) body.password = values.password;
        if (values.role !== editing.role) body.role = values.role;
        if (Number(values.policy_id) !== editing.policy_id) body.policy_id = Number(values.policy_id);
        if (values.notes !== (editing.notes ?? "")) body.notes = values.notes || null;
        return updateUser(editing.id, body);
      }
      body.username = values.username;
      if (values.display_name) body.display_name = values.display_name;
      if (values.password) body.password = values.password;
      body.role = values.role;
      body.policy_id = Number(values.policy_id);
      if (values.notes) body.notes = values.notes;
      return createUser(body);
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: qk.users });
      queryClient.invalidateQueries({ queryKey: qk.overview });
      notifications.show({ message: editing ? "用户已更新" : "用户已创建", color: "green" });
      onClose();
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "操作失败", color: "red" }),
  });

  const policyData = policies.map((p) => ({ value: String(p.id), label: p.name }));

  return (
    <Modal opened={opened} onClose={onClose} title={editing ? "编辑用户" : "新建用户"}>
      <form onSubmit={form.onSubmit((v) => mutation.mutate(v))}>
        <Stack>
          <TextInput label="用户名" required key={form.key("username")} {...form.getInputProps("username")} />
          <TextInput label="显示名称" key={form.key("display_name")} {...form.getInputProps("display_name")} />
          <PasswordInput
            label={editing ? "新密码（留空不修改）" : "密码"}
            key={form.key("password")}
            {...form.getInputProps("password")}
          />
          <Select
            label="角色"
            data={[
              { value: "admin", label: "管理员" },
              { value: "member", label: "成员" },
            ]}
            key={form.key("role")}
            {...form.getInputProps("role")}
          />
          <Select label="策略" data={policyData} key={form.key("policy_id")} {...form.getInputProps("policy_id")} />
          <TextInput label="备注" key={form.key("notes")} {...form.getInputProps("notes")} />
          <Group justify="flex-end">
            <Button variant="default" onClick={onClose}>取消</Button>
            <Button type="submit" loading={mutation.isPending}>{editing ? "保存" : "创建"}</Button>
          </Group>
        </Stack>
      </form>
    </Modal>
  );
}

// ─── Delete Confirm ───

function DeleteConfirm({
  label,
  item,
  onClose,
  onConfirm,
  loading,
}: {
  label: string;
  item: { id: number } | null;
  onClose: () => void;
  onConfirm: () => void;
  loading: boolean;
}) {
  return (
    <Modal opened={!!item} onClose={onClose} title={`删除${label}`}>
      <Stack>
        <Text>确定要删除吗？此操作不可恢复。</Text>
        <Group justify="flex-end">
          <Button variant="default" onClick={onClose}>取消</Button>
          <Button color="red" loading={loading} onClick={onConfirm}>删除</Button>
        </Group>
      </Stack>
    </Modal>
  );
}

// ─── Policy Form Modal ───

function PolicyFormModal({
  opened,
  onClose,
  editing,
}: {
  opened: boolean;
  onClose: () => void;
  editing: Policy | null;
}) {
  const queryClient = useQueryClient();
  const NANO = 1_000_000_000;
  const form = useForm({
    mode: "uncontrolled",
    initialValues: {
      name: editing?.name ?? "",
      max_concurrent: editing?.max_concurrent ?? 2,
      rpm_limit: editing?.rpm_limit ?? 10,
      weekly_budget: editing ? editing.weekly_budget_nanousd / NANO : 50,
      monthly_budget: editing ? editing.monthly_budget_nanousd / NANO : 200,
    },
    validate: {
      name: (v) => (v.trim() ? null : "必填"),
      max_concurrent: (v) => (v > 0 ? null : "必须大于 0"),
      rpm_limit: (v) => (v > 0 ? null : "必须大于 0"),
    },
  });

  const mutation = useMutation({
    mutationFn: async (values: typeof form.values) => {
      const body: Record<string, unknown> = {
        name: values.name,
        max_concurrent: values.max_concurrent,
        rpm_limit: values.rpm_limit,
        weekly_budget_nanousd: Math.round(values.weekly_budget * NANO),
        monthly_budget_nanousd: Math.round(values.monthly_budget * NANO),
      };
      if (editing) return updatePolicy(editing.id, body);
      return createPolicy(body);
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: qk.policies });
      notifications.show({ message: editing ? "策略已更新" : "策略已创建", color: "green" });
      onClose();
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "操作失败", color: "red" }),
  });

  return (
    <Modal opened={opened} onClose={onClose} title={editing ? "编辑策略" : "新建策略"}>
      <form onSubmit={form.onSubmit((v) => mutation.mutate(v))}>
        <Stack>
          <TextInput label="名称" required key={form.key("name")} {...form.getInputProps("name")} />
          <NumberInput label="最大并发" min={1} key={form.key("max_concurrent")} {...form.getInputProps("max_concurrent")} />
          <NumberInput label="RPM 限制" min={1} key={form.key("rpm_limit")} {...form.getInputProps("rpm_limit")} />
          <NumberInput label="周预算 ($)" min={0} decimalScale={2} key={form.key("weekly_budget")} {...form.getInputProps("weekly_budget")} />
          <NumberInput label="月预算 ($)" min={0} decimalScale={2} key={form.key("monthly_budget")} {...form.getInputProps("monthly_budget")} />
          <Group justify="flex-end">
            <Button variant="default" onClick={onClose}>取消</Button>
            <Button type="submit" loading={mutation.isPending}>{editing ? "保存" : "创建"}</Button>
          </Group>
        </Stack>
      </form>
    </Modal>
  );
}

// ─── Users Tab ───

function UsersTab({ policies }: { policies: Policy[] }) {
  const queryClient = useQueryClient();
  const { data, isLoading, error } = useQuery({ queryKey: qk.users, queryFn: listUsers });
  const [formOpened, setFormOpened] = useState(false);
  const [editing, setEditing] = useState<UserRow | null>(null);
  const [deleting, setDeleting] = useState<UserRow | null>(null);

  const toggleMut = useMutation({
    mutationFn: ({ id, disabled }: { id: number; disabled: boolean }) =>
      updateUser(id, { disabled }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: qk.users });
      queryClient.invalidateQueries({ queryKey: qk.overview });
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "操作失败", color: "red" }),
  });

  const deleteMut = useMutation({
    mutationFn: () => deleteUser(deleting!.id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: qk.users });
      queryClient.invalidateQueries({ queryKey: qk.overview });
      notifications.show({ message: "用户已删除", color: "green" });
      setDeleting(null);
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "操作失败", color: "red" }),
  });

  if (isLoading) return <Skeleton height={300} />;
  if (error) return <Alert color="red">{String(error)}</Alert>;

  const users = data?.items ?? [];

  return (
    <>
      <Group justify="flex-end" mb="md">
        <Button leftSection={<IconPlus size={16} />} onClick={() => { setEditing(null); setFormOpened(true); }}>
          添加用户
        </Button>
      </Group>

      {users.length === 0 ? (
        <Text c="dimmed">暂无用户。</Text>
      ) : (
        <ScrollArea>
          <Table striped highlightOnHover verticalSpacing="sm">
            <Table.Thead>
              <Table.Tr>
                <Table.Th>用户名</Table.Th>
                <Table.Th>角色</Table.Th>
                <Table.Th>策略</Table.Th>
                <Table.Th visibleFrom="md">Key</Table.Th>
                <Table.Th>本周</Table.Th>
                <Table.Th>本月</Table.Th>
                <Table.Th visibleFrom="md">最后活跃</Table.Th>
                <Table.Th w={140}>操作</Table.Th>
              </Table.Tr>
            </Table.Thead>
            <Table.Tbody>
              {users.map((u) => (
                <Table.Tr key={u.id}>
                  <Table.Td>
                    <Group gap="xs">
                      <Text size="sm">{u.username}</Text>
                      {u.disabled_at && <Badge color="gray" size="xs">已禁用</Badge>}
                    </Group>
                  </Table.Td>
                  <Table.Td><Badge color={u.role === "admin" ? "blue" : "gray"} variant="light">{u.role === "admin" ? "管理员" : "成员"}</Badge></Table.Td>
                  <Table.Td>{u.policy_name}</Table.Td>
                  <Table.Td visibleFrom="md">{u.key_count}</Table.Td>
                  <Table.Td>{formatCost(u.current_week_cost_nanousd)}</Table.Td>
                  <Table.Td>{formatCost(u.current_month_cost_nanousd)}</Table.Td>
                  <Table.Td visibleFrom="md">{formatDate(u.last_seen_at)}</Table.Td>
                  <Table.Td>
                    <Group gap={4}>
                      <Tooltip label="编辑">
                        <ActionIcon variant="subtle" onClick={() => { setEditing(u); setFormOpened(true); }}>
                          <IconEdit size={16} />
                        </ActionIcon>
                      </Tooltip>
                      <Tooltip label={u.disabled_at ? "启用" : "禁用"}>
                        <ActionIcon
                          variant="subtle"
                          color={u.disabled_at ? "green" : "yellow"}
                          onClick={() => toggleMut.mutate({ id: u.id, disabled: !u.disabled_at })}
                        >
                          {u.disabled_at ? <IconPlayerPlay size={16} /> : <IconPlayerPause size={16} />}
                        </ActionIcon>
                      </Tooltip>
                      <Tooltip label="删除">
                        <ActionIcon variant="subtle" color="red" onClick={() => setDeleting(u)}>
                          <IconTrash size={16} />
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

      <UserFormModal
        key={editing?.id ?? "new"}
        opened={formOpened}
        onClose={() => setFormOpened(false)}
        editing={editing}
        policies={policies}
      />
      <DeleteConfirm
        label="用户"
        item={deleting}
        onClose={() => setDeleting(null)}
        onConfirm={() => deleteMut.mutate()}
        loading={deleteMut.isPending}
      />
    </>
  );
}

// ─── Policies Tab ───

function PoliciesTab() {
  const queryClient = useQueryClient();
  const { data, isLoading, error } = useQuery({ queryKey: qk.policies, queryFn: listPolicies });
  const [formOpened, setFormOpened] = useState(false);
  const [editing, setEditing] = useState<Policy | null>(null);
  const [deleting, setDeleting] = useState<Policy | null>(null);

  const deleteMut = useMutation({
    mutationFn: () => deletePolicy(deleting!.id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: qk.policies });
      notifications.show({ message: "策略已删除", color: "green" });
      setDeleting(null);
    },
    onError: (e) =>
      notifications.show({ message: e instanceof ApiError ? e.message : "操作失败", color: "red" }),
  });

  if (isLoading) return <Skeleton height={200} />;
  if (error) return <Alert color="red">{String(error)}</Alert>;

  const policies = data?.items ?? [];

  return (
    <>
      <Group justify="flex-end" mb="md">
        <Button leftSection={<IconPlus size={16} />} onClick={() => { setEditing(null); setFormOpened(true); }}>
          添加策略
        </Button>
      </Group>

      {policies.length === 0 ? (
        <Text c="dimmed">暂无策略。</Text>
      ) : (
        <ScrollArea>
          <Table striped highlightOnHover verticalSpacing="sm">
            <Table.Thead>
              <Table.Tr>
                <Table.Th>名称</Table.Th>
                <Table.Th>并发</Table.Th>
                <Table.Th>RPM</Table.Th>
                <Table.Th>周预算</Table.Th>
                <Table.Th>月预算</Table.Th>
                <Table.Th>用户数</Table.Th>
                <Table.Th w={80}>操作</Table.Th>
              </Table.Tr>
            </Table.Thead>
            <Table.Tbody>
              {policies.map((p) => (
                <Table.Tr key={p.id}>
                  <Table.Td>{p.name}</Table.Td>
                  <Table.Td>{p.max_concurrent}</Table.Td>
                  <Table.Td>{p.rpm_limit}</Table.Td>
                  <Table.Td>{formatCost(p.weekly_budget_nanousd)}</Table.Td>
                  <Table.Td>{formatCost(p.monthly_budget_nanousd)}</Table.Td>
                  <Table.Td>{p.assigned_user_count}</Table.Td>
                  <Table.Td>
                    <Group gap={4}>
                      <ActionIcon variant="subtle" onClick={() => { setEditing(p); setFormOpened(true); }}>
                        <IconEdit size={16} />
                      </ActionIcon>
                      <Tooltip label={p.assigned_user_count > 0 ? "有关联用户" : "删除"}>
                        <ActionIcon
                          variant="subtle"
                          color="red"
                          disabled={p.assigned_user_count > 0}
                          onClick={() => setDeleting(p)}
                        >
                          <IconTrash size={16} />
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

      <PolicyFormModal
        key={editing?.id ?? "new"}
        opened={formOpened}
        onClose={() => setFormOpened(false)}
        editing={editing}
      />
      <DeleteConfirm
        label="策略"
        item={deleting}
        onClose={() => setDeleting(null)}
        onConfirm={() => deleteMut.mutate()}
        loading={deleteMut.isPending}
      />
    </>
  );
}

// ─── Main Page ───

export default function Users() {
  const { data: policiesData } = useQuery({ queryKey: qk.policies, queryFn: listPolicies });
  const policies = policiesData?.items ?? [];

  return (
    <>
      <Title order={3} mb="md">用户与策略</Title>
      <Tabs defaultValue="users">
        <Tabs.List>
          <Tabs.Tab value="users">用户</Tabs.Tab>
          <Tabs.Tab value="policies">策略</Tabs.Tab>
        </Tabs.List>
        <Tabs.Panel value="users" pt="md">
          <UsersTab policies={policies} />
        </Tabs.Panel>
        <Tabs.Panel value="policies" pt="md">
          <PoliciesTab />
        </Tabs.Panel>
      </Tabs>
    </>
  );
}
