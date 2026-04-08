import { useEffect, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Title,
  Table,
  Badge,
  Button,
  Group,
  Select,
  Text,
  Skeleton,
  Alert,
  ScrollArea,
  Drawer,
  Code,
} from "@mantine/core";
import { IconChevronLeft, IconChevronRight } from "@tabler/icons-react";
import { listModelsAdmin, listRequests, listUsers, qk, type RequestLog, type RequestFilters } from "../api";
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
    ["开始时间", formatDate(log.started_at)],
    ["完成时间", formatDate(log.completed_at)],
    ["首字耗时", log.ttft_ms != null ? `${log.ttft_ms}ms` : "—"],
    ["总耗时", log.duration_ms != null ? `${log.duration_ms}ms` : "—"],
    ["状态", <Badge key="st" color={statusColor(log.status)} variant="light">{log.status}</Badge>],
    ["HTTP 状态", log.http_status != null ? String(log.http_status) : "—"],
    ["输入 Token", log.input_tokens?.toLocaleString() ?? "—"],
    ["输出 Token", log.output_tokens?.toLocaleString() ?? "—"],
    ["缓存创建", log.cache_creation_tokens?.toLocaleString() ?? "—"],
    ["缓存读取", log.cache_read_tokens?.toLocaleString() ?? "—"],
    ["费用", log.request_type === "count_tokens" ? `≈${formatCost(log.cost_nanousd)}` : formatCost(log.cost_nanousd)],
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
  const queryClient = useQueryClient();

  const { data: usersData } = useQuery({ queryKey: qk.users, queryFn: listUsers });
  const userData = usersData?.items?.map((u) => ({ value: String(u.id), label: u.username })) ?? [];

  const { data: modelsData } = useQuery({ queryKey: qk.models, queryFn: listModelsAdmin });
  const modelData = modelsData?.items?.map((m) => ({ value: m.model_id, label: m.display_name })) ?? [];

  const { data, isLoading, error } = useQuery({
    queryKey: qk.requests(filters),
    queryFn: () => listRequests(filters),
    refetchInterval: 60_000,
  });

  const offset = filters.offset ?? 0;
  const reconnectTimer = useRef<ReturnType<typeof setTimeout>>(null);
  useEffect(() => {
    if (offset > 0) return;
    let disposed = false;
    let es: EventSource | null = null;

    function connect() {
      if (disposed) return;
      es = new EventSource("/api/admin/events");
      es.onmessage = (event) => {
        try {
          const payload = JSON.parse(event.data) as { topic?: string };
          if (!payload.topic || payload.topic === "request_logs") {
            queryClient.invalidateQueries({ queryKey: ["requests"] });
          }
        } catch {
          queryClient.invalidateQueries({ queryKey: ["requests"] });
        }
      };
      es.onerror = () => {
        es?.close();
        es = null;
        if (!disposed) reconnectTimer.current = setTimeout(connect, 5000);
      };
    }
    connect();

    return () => {
      disposed = true;
      es?.close();
      if (reconnectTimer.current) clearTimeout(reconnectTimer.current);
    };
  }, [offset, queryClient]);

  const logs = data?.items ?? [];
  const total = data?.total ?? 0;
  const hasNext = offset + PAGE_SIZE < total;
  const hasPrev = offset > 0;

  const updateFilter = (key: string, value: string | number | undefined) => {
    setFilters((f) => ({ ...f, [key]: value || undefined, offset: 0 }));
  };

  const setTimeRange = (range: string | null) => {
    if (!range) {
      setFilters((f) => ({ ...f, started_from: undefined, started_to: undefined, offset: 0 }));
      return;
    }
    const now = new Date();
    const shanghaiNow = new Date(now.toLocaleString("en-US", { timeZone: "Asia/Shanghai" }));
    const from = new Date(shanghaiNow);
    if (range === "today") from.setHours(0, 0, 0, 0);
    else if (range === "7d") from.setDate(from.getDate() - 7);
    else if (range === "30d") from.setDate(from.getDate() - 30);
    const offset = shanghaiNow.getTime() - now.getTime();
    const utcFrom = new Date(from.getTime() - offset);
    setFilters((f) => ({
      ...f,
      started_from: utcFrom.toISOString(),
      started_to: undefined,
      offset: 0,
    }));
  };

  return (
    <>
      <Title order={3} mb="md">请求日志</Title>

      <Group mb="md" gap="sm" align="end">
        <Select
          label="类型"
          placeholder="全部"
          data={[
            { value: "messages", label: "messages" },
            { value: "count_tokens", label: "count_tokens" },
          ]}
          value={filters.request_type ?? null}
          onChange={(v) => updateFilter("request_type", v ?? undefined)}
          clearable
          size="sm"
          w={150}
        />
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
            { value: "internal_error", label: "内部错误" },
          ]}
          value={filters.status ?? null}
          onChange={(v) => updateFilter("status", v ?? undefined)}
          clearable
          size="sm"
          w={130}
        />
        <Select
          label="模型"
          placeholder="全部"
          data={modelData}
          value={filters.model ?? null}
          onChange={(v) => updateFilter("model", v ?? undefined)}
          clearable
          searchable
          size="sm"
          w={200}
        />
        <Select
          label="时间"
          placeholder="全部"
          data={[
            { value: "today", label: "今天" },
            { value: "7d", label: "近 7 天" },
            { value: "30d", label: "近 30 天" },
          ]}
          value={
            filters.started_from
              ? (new Date().getTime() - new Date(filters.started_from).getTime() < 86400_000 ? "today"
                : new Date().getTime() - new Date(filters.started_from).getTime() < 7 * 86400_000 + 60_000 ? "7d"
                : "30d")
              : null
          }
          onChange={setTimeRange}
          clearable
          size="sm"
          w={120}
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
                  <Table.Th>类型</Table.Th>
                  <Table.Th>用户</Table.Th>
                  <Table.Th>模型</Table.Th>
                  <Table.Th>状态</Table.Th>
                  <Table.Th visibleFrom="md">首字</Table.Th>
                  <Table.Th visibleFrom="md">总耗时</Table.Th>
                  <Table.Th visibleFrom="md">Token</Table.Th>
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
                    <Table.Td>
                      <Badge variant="outline" size="sm">
                        {log.request_type}
                      </Badge>
                    </Table.Td>
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
                      {log.ttft_ms != null ? `${(log.ttft_ms / 1000).toFixed(1)}s` : "—"}
                    </Table.Td>
                    <Table.Td visibleFrom="md">
                      {log.duration_ms != null ? `${(log.duration_ms / 1000).toFixed(1)}s` : "—"}
                    </Table.Td>
                    <Table.Td visibleFrom="md">
                      <Text size="xs">
                        {log.request_type === "count_tokens"
                          ? (log.input_tokens?.toLocaleString() ?? "—")
                          : log.input_tokens != null
                            ? `${log.input_tokens.toLocaleString()}→${(log.output_tokens ?? 0).toLocaleString()}`
                            : "—"}
                        {log.request_type !== "count_tokens" && (log.cache_creation_tokens || log.cache_read_tokens)
                          ? ` (w${(log.cache_creation_tokens ?? 0).toLocaleString()}/r${(log.cache_read_tokens ?? 0).toLocaleString()})`
                          : ""}
                      </Text>
                    </Table.Td>
                    <Table.Td>
                      {log.request_type === "count_tokens" ? `≈${formatCost(log.cost_nanousd)}` : formatCost(log.cost_nanousd)}
                    </Table.Td>
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
