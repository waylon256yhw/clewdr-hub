import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  Title,
  Table,
  Badge,
  Button,
  Group,
  TextInput,
  Select,
  Text,
  Skeleton,
  Alert,
  ScrollArea,
  Drawer,
  Code,
} from "@mantine/core";
import { IconChevronLeft, IconChevronRight, IconFilter } from "@tabler/icons-react";
import { listRequests, listUsers, qk, type RequestLog, type RequestFilters } from "../api";
import { formatCost, formatDate, statusColor } from "../lib/format";

const PAGE_SIZE = 50;

function LogDetail({ log, onClose }: { log: RequestLog | null; onClose: () => void }) {
  if (!log) return null;

  const rows: [string, React.ReactNode][] = [
    ["请求 ID", <Code key="rid">{log.request_id}</Code>],
    ["类型", log.request_type],
    ["用户", log.username ?? "—"],
    ["Key", log.key_label ?? "—"],
    ["账号", log.account_name ?? "—"],
    ["模型", log.model_raw],
    ["模型 (标准化)", log.model_normalized ?? "—"],
    ["流式", log.stream ? "是" : "否"],
    ["开始时间", log.started_at],
    ["完成时间", log.completed_at ?? "—"],
    ["耗时", log.duration_ms != null ? `${log.duration_ms}ms` : "—"],
    ["状态", <Badge key="st" color={statusColor(log.status)} variant="light">{log.status}</Badge>],
    ["HTTP 状态", log.http_status != null ? String(log.http_status) : "—"],
    ["输入 Token", log.input_tokens?.toLocaleString() ?? "—"],
    ["输出 Token", log.output_tokens?.toLocaleString() ?? "—"],
    ["费用", formatCost(log.cost_nanousd)],
    ["错误码", log.error_code ?? "—"],
    ["错误信息", log.error_message ?? "—"],
  ];

  return (
    <Drawer opened={!!log} onClose={onClose} title="请求详情" position="right" size="md">
      <Table>
        <Table.Tbody>
          {rows.map(([label, value]) => (
            <Table.Tr key={label}>
              <Table.Td fw={600} w={120}>{label}</Table.Td>
              <Table.Td>{value}</Table.Td>
            </Table.Tr>
          ))}
        </Table.Tbody>
      </Table>
    </Drawer>
  );
}

export default function Logs() {
  const [filters, setFilters] = useState<RequestFilters>({ offset: 0, limit: PAGE_SIZE });
  const [detail, setDetail] = useState<RequestLog | null>(null);

  const { data: usersData } = useQuery({ queryKey: qk.users, queryFn: listUsers });
  const userData = usersData?.items?.map((u) => ({ value: String(u.id), label: u.username })) ?? [];

  const { data, isLoading, error } = useQuery({
    queryKey: qk.requests(filters),
    queryFn: () => listRequests(filters),
  });

  const logs = data?.items ?? [];
  const total = data?.total ?? 0;
  const offset = filters.offset ?? 0;
  const hasNext = offset + PAGE_SIZE < total;
  const hasPrev = offset > 0;

  const updateFilter = (key: string, value: string | number | undefined) => {
    setFilters((f) => ({ ...f, [key]: value || undefined, offset: 0 }));
  };

  return (
    <>
      <Title order={3} mb="md">请求日志</Title>

      <Group mb="md" gap="sm" align="end">
        <Select
          label="用户"
          placeholder="全部"
          data={userData}
          value={filters.user_id != null ? String(filters.user_id) : null}
          onChange={(v) => updateFilter("user_id", v ? Number(v) : undefined)}
          clearable
          size="sm"
          w={150}
        />
        <Select
          label="状态"
          placeholder="全部"
          data={[
            { value: "ok", label: "成功" },
            { value: "upstream_error", label: "上游错误" },
            { value: "client_abort", label: "客户端中断" },
            { value: "auth_rejected", label: "认证拒绝" },
            { value: "quota_rejected", label: "配额超限" },
            { value: "user_concurrency_rejected", label: "并发超限" },
            { value: "rpm_rejected", label: "RPM 超限" },
            { value: "no_account_available", label: "无可用账号" },
          ]}
          value={filters.status ?? null}
          onChange={(v) => updateFilter("status", v ?? undefined)}
          clearable
          size="sm"
          w={130}
        />
        <TextInput
          label="模型"
          placeholder="筛选..."
          value={filters.model ?? ""}
          onChange={(e) => updateFilter("model", e.currentTarget.value)}
          size="sm"
          w={150}
          leftSection={<IconFilter size={14} />}
        />
      </Group>

      {isLoading ? (
        <Skeleton height={300} />
      ) : error ? (
        <Alert color="red">{String(error)}</Alert>
      ) : logs.length === 0 ? (
        <Text c="dimmed">暂无日志。</Text>
      ) : (
        <>
          <ScrollArea>
            <Table striped highlightOnHover verticalSpacing="sm">
              <Table.Thead>
                <Table.Tr>
                  <Table.Th>时间</Table.Th>
                  <Table.Th>用户</Table.Th>
                  <Table.Th>模型</Table.Th>
                  <Table.Th>状态</Table.Th>
                  <Table.Th visibleFrom="md">耗时</Table.Th>
                  <Table.Th visibleFrom="md">输入</Table.Th>
                  <Table.Th visibleFrom="md">输出</Table.Th>
                  <Table.Th>费用</Table.Th>
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {logs.map((log) => (
                  <Table.Tr
                    key={log.id}
                    style={{ cursor: "pointer" }}
                    onClick={() => setDetail(log)}
                  >
                    <Table.Td>{formatDate(log.started_at)}</Table.Td>
                    <Table.Td>{log.username ?? "—"}</Table.Td>
                    <Table.Td>
                      <Text size="xs" lineClamp={1}>{log.model_raw}</Text>
                    </Table.Td>
                    <Table.Td>
                      <Badge color={statusColor(log.status)} variant="light" size="sm">
                        {log.status}
                      </Badge>
                    </Table.Td>
                    <Table.Td visibleFrom="md">
                      {log.duration_ms != null ? `${log.duration_ms}ms` : "—"}
                    </Table.Td>
                    <Table.Td visibleFrom="md">{log.input_tokens?.toLocaleString() ?? "—"}</Table.Td>
                    <Table.Td visibleFrom="md">{log.output_tokens?.toLocaleString() ?? "—"}</Table.Td>
                    <Table.Td>{formatCost(log.cost_nanousd)}</Table.Td>
                  </Table.Tr>
                ))}
              </Table.Tbody>
            </Table>
          </ScrollArea>

          <Group justify="space-between" mt="md">
            <Text size="sm" c="dimmed">
              {offset + 1}–{Math.min(offset + PAGE_SIZE, total)} / 共 {total} 条
            </Text>
            <Group gap="xs">
              <Button
                variant="default"
                size="xs"
                disabled={!hasPrev}
                onClick={() => setFilters((f) => ({ ...f, offset: Math.max(0, offset - PAGE_SIZE) }))}
                leftSection={<IconChevronLeft size={14} />}
              >
                上一页
              </Button>
              <Button
                variant="default"
                size="xs"
                disabled={!hasNext}
                onClick={() => setFilters((f) => ({ ...f, offset: offset + PAGE_SIZE }))}
                rightSection={<IconChevronRight size={14} />}
              >
                下一页
              </Button>
            </Group>
          </Group>
        </>
      )}

      <LogDetail log={detail} onClose={() => setDetail(null)} />
    </>
  );
}
