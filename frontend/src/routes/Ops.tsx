import { useState } from "react";
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { DonutChart, LineChart } from "@mantine/charts";
import {
  Alert,
  Badge,
  Group,
  Paper,
  SegmentedControl,
  Select,
  SimpleGrid,
  Skeleton,
  Stack,
  Table,
  Text,
  Title,
} from "@mantine/core";
import { useNavigate } from "react-router";
import {
  getOpsUsage,
  listUsers,
  qk,
  type ModelDistributionItem,
  type OpsUsageResponse,
  type UserSeriesPoint,
} from "../api";
import {
  formatCompactCount,
  formatCost,
  formatShanghaiBucket,
  formatTokenCount,
} from "../lib/format";

type OpsRange = "24h" | "7d" | "30d";
type OpsMetric = "cost" | "tokens" | "requests";
const USER_FILTER_WIDTH = 140;
const TOP_USERS_FILTER_WIDTH = 168;

const CHART_COLORS = [
  "blue.6",
  "cyan.6",
  "teal.6",
  "green.6",
  "lime.6",
  "yellow.6",
  "orange.6",
  "red.6",
];

function metricValueFromModel(item: ModelDistributionItem, metric: OpsMetric): number {
  if (metric === "cost") return item.cost_nanousd / 1_000_000_000;
  if (metric === "tokens") return item.total_tokens;
  return item.request_count;
}

function metricValueFromPoint(point: UserSeriesPoint, metric: OpsMetric): number {
  if (metric === "cost") return point.cost_nanousd / 1_000_000_000;
  if (metric === "tokens") return point.total_tokens;
  return point.request_count;
}

function formatMetric(metric: OpsMetric, value: number): string {
  if (metric === "cost") return `$${value.toFixed(value < 0.01 ? 4 : 2)}`;
  if (metric === "tokens") return formatTokenCount(Math.round(value));
  return formatCompactCount(Math.round(value));
}

function KpiCard({
  label,
  value,
  hint,
}: {
  label: string;
  value: string;
  hint?: React.ReactNode;
}) {
  return (
    <Paper shadow="xs" p="md" radius="md" withBorder>
      <Text size="sm" c="dimmed" mb={6}>{label}</Text>
      <Text fw={700} size="xl">{value}</Text>
      {hint ? <Text size="xs" c="dimmed" mt={4}>{hint}</Text> : null}
    </Paper>
  );
}

export default function Ops() {
  const navigate = useNavigate();
  const [range, setRange] = useState<OpsRange>("7d");
  const [metric, setMetric] = useState<OpsMetric>("cost");
  const [topUsers, setTopUsers] = useState<string | null>(null);
  const [userFilter, setUserFilter] = useState<string | null>(null);

  const selectedUserId = userFilter ? Number(userFilter) : undefined;
  const topUsersValue = topUsers ? Number(topUsers) : 5;
  const { data: usersData } = useQuery({ queryKey: qk.users, queryFn: listUsers });
  const userData = usersData?.items?.map((u) => ({ value: String(u.id), label: u.username })) ?? [];

  const { data, isLoading, error } = useQuery({
    queryKey: qk.opsUsage(range, topUsersValue, selectedUserId),
    queryFn: () => getOpsUsage(range, topUsersValue, selectedUserId),
    refetchInterval: 60_000,
    placeholderData: keepPreviousData,
  });

  if (isLoading) {
    return (
      <>
        <Title order={3} mb="md">运维</Title>
        <SimpleGrid cols={{ base: 1, sm: 2, xl: 4 }} spacing="md">
          {Array.from({ length: 4 }).map((_, index) => (
            <Skeleton key={index} height={108} radius="md" />
          ))}
        </SimpleGrid>
        <SimpleGrid cols={{ base: 1, xl: 2 }} spacing="md" mt="md">
          <Skeleton height={360} radius="md" />
          <Skeleton height={360} radius="md" />
        </SimpleGrid>
      </>
    );
  }

  if (error || !data) {
    return (
      <Alert color="red" title="运维数据加载失败">
        {String(error ?? "未知错误")}
      </Alert>
    );
  }

  const donutData = data.model_distribution.map((item, index) => ({
    name: item.model,
    value: metricValueFromModel(item, metric),
    color: CHART_COLORS[index % CHART_COLORS.length],
  }));

  const lineData = buildLineData(data, metric);
  const lineSeries = data.user_series.map((item, index) => ({
    name: item.username,
    color: CHART_COLORS[index % CHART_COLORS.length],
  }));
  const userIdByUsername = new Map(data.user_series.map((item) => [item.username, item.user_id]));

  const toLogs = (input: {
    startedFrom: string;
    startedTo: string;
    userId?: number;
    model?: string;
  }) => {
    const search = new URLSearchParams();
    search.set("request_type", "messages");
    search.set("started_from", input.startedFrom);
    search.set("started_to", input.startedTo);
    if (input.userId != null) search.set("user_id", String(input.userId));
    if (input.model) search.set("model", input.model);
    navigate(`/logs?${search.toString()}`);
  };

  return (
    <>
      <Title order={3} mb="md">运维</Title>

      <Group mb="md" align="end" gap="sm">
        <SegmentedControl
          value={range}
          onChange={(value) => setRange(value as OpsRange)}
          data={[
            { label: "近 24h", value: "24h" },
            { label: "近 7 天", value: "7d" },
            { label: "近 30 天", value: "30d" },
          ]}
        />
        <SegmentedControl
          value={metric}
          onChange={(value) => setMetric(value as OpsMetric)}
          data={[
            { label: "金额", value: "cost" },
            { label: "Token", value: "tokens" },
            { label: "请求数", value: "requests" },
          ]}
        />
        <Select
          placeholder="用户筛选"
          aria-label="用户筛选"
          data={userData}
          value={userFilter}
          onChange={(value) => setUserFilter(value)}
          clearable
          searchable
          w={USER_FILTER_WIDTH}
        />
        <Select
          placeholder="折线图用户数"
          aria-label="折线图用户数"
          data={[
            { value: "3", label: "Top 3" },
            { value: "5", label: "Top 5" },
            { value: "8", label: "Top 8" },
          ]}
          value={topUsers}
          onChange={(value) => value && setTopUsers(value)}
          w={TOP_USERS_FILTER_WIDTH}
        />
      </Group>

      <SimpleGrid cols={{ base: 1, sm: 2, xl: 4 }} spacing="md" mb="md">
        <KpiCard label="累计请求数" value={formatCompactCount(data.totals.request_count)} />
        <KpiCard
          label="累计 Token"
          value={formatTokenCount(data.totals.total_tokens)}
          hint={(
            <Group gap={4} wrap="nowrap" mt={4}>
              <Badge
                size="sm"
                variant="light"
                color="cyan"
                radius="sm"
                title="输入 token"
              >
                ↑{formatTokenCount(data.totals.input_tokens)}
              </Badge>
              <Badge
                size="sm"
                variant="light"
                color="teal"
                radius="sm"
                title="输出 token"
              >
                ↓{formatTokenCount(data.totals.output_tokens)}
              </Badge>
              <Badge
                size="sm"
                variant="light"
                color="grape"
                radius="sm"
                title="缓存写入 (1.25× 输入价)"
              >
                +{formatTokenCount(data.totals.cache_creation_tokens)}
              </Badge>
              <Badge
                size="sm"
                variant="light"
                color="gray"
                radius="sm"
                title="缓存读取 (0.10× 输入价)"
              >
                ↻{formatTokenCount(data.totals.cache_read_tokens)}
              </Badge>
            </Group>
          )}
        />
        <KpiCard
          label="累计金额"
          value={formatCost(data.totals.cost_nanousd)}
        />
        <KpiCard
          label="分析窗口"
          value={range === "24h" ? "近 24 小时" : range === "7d" ? "近 7 天" : "近 30 天"}
          hint={`图表口径：${metric === "cost" ? "金额" : metric === "tokens" ? "Token" : "请求数"}`}
        />
      </SimpleGrid>

      <SimpleGrid cols={{ base: 1, xl: 2 }} spacing="md" mb="md">
        <Paper shadow="xs" p="md" radius="md" withBorder>
          <Stack gap="xs">
            <Group justify="space-between">
              <Text fw={600}>模型分布</Text>
              <Badge variant="light">{metric === "cost" ? "按金额" : metric === "tokens" ? "按 Token" : "按请求数"}</Badge>
            </Group>
            {donutData.length === 0 ? (
              <Text c="dimmed" size="sm">当前窗口没有可展示的数据。</Text>
            ) : (
              <DonutChart
                data={donutData}
                h={320}
                withLabelsLine
                labelsType="percent"
                withTooltip
                valueFormatter={(value) => formatMetric(metric, value)}
                cellProps={(series) => ({
                  style: { cursor: "pointer" },
                  onClick: () =>
                    toLogs({
                      startedFrom: data.window_started_at,
                      startedTo: data.window_ended_at,
                      userId: selectedUserId,
                      model: series.name !== "unknown" ? series.name : undefined,
                    }),
                })}
              />
            )}
          </Stack>
        </Paper>

        <Paper shadow="xs" p="md" radius="md" withBorder>
          <Stack gap="xs">
            <Group justify="space-between">
              <Text fw={600}>各用户用量跟踪</Text>
              <Badge variant="light">{`Top ${topUsersValue}`}</Badge>
            </Group>
            {lineSeries.length === 0 ? (
              <Text c="dimmed" size="sm">当前窗口没有用户使用数据。</Text>
            ) : (
              <LineChart
                h={320}
                data={lineData}
                series={lineSeries}
                curveType="linear"
                withLegend
                dataKey="bucketRaw"
                xAxisProps={{
                  tickFormatter: (value) =>
                    formatShanghaiBucket(String(value), data.bucket_unit),
                }}
                lineChartProps={{
                  onClick: (state) => {
                    if (!state.activeLabel || !state.activeDataKey) return;
                    const bucket = String(state.activeLabel);
                    const username = String(state.activeDataKey);
                    const userId = userIdByUsername.get(username);
                    if (!userId) return;
                    const { startedFrom, startedTo } = bucketToUtcRange(bucket, data.bucket_unit);
                    toLogs({
                      startedFrom,
                      startedTo,
                      userId,
                    });
                  },
                }}
                valueFormatter={(value) => formatMetric(metric, Number(value))}
              />
            )}
          </Stack>
        </Paper>
      </SimpleGrid>

      <Paper shadow="xs" p="md" radius="md" withBorder>
        <Stack gap="sm">
          <Text fw={600}>用户排行</Text>
          {data.top_users.length === 0 ? (
            <Text c="dimmed" size="sm">当前窗口没有排行数据。</Text>
          ) : (
            <Table striped highlightOnHover>
              <Table.Thead>
                <Table.Tr>
                  <Table.Th>用户</Table.Th>
                  <Table.Th>请求数</Table.Th>
                  <Table.Th>总 Token</Table.Th>
                  <Table.Th>金额</Table.Th>
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {data.top_users.map((item) => (
                  <Table.Tr
                    key={item.user_id}
                    style={{ cursor: "pointer" }}
                    onClick={() =>
                      toLogs({
                        startedFrom: data.window_started_at,
                        startedTo: data.window_ended_at,
                        userId: item.user_id,
                      })
                    }
                  >
                    <Table.Td>{item.username}</Table.Td>
                    <Table.Td>{item.request_count.toLocaleString("zh-CN")}</Table.Td>
                    <Table.Td>{formatTokenCount(item.total_tokens)}</Table.Td>
                    <Table.Td>{formatCost(item.cost_nanousd)}</Table.Td>
                  </Table.Tr>
                ))}
              </Table.Tbody>
            </Table>
          )}
        </Stack>
      </Paper>
    </>
  );
}

function buildLineData(data: OpsUsageResponse, metric: OpsMetric): Record<string, string | number>[] {
  return data.buckets.map((bucket) => {
    const row: Record<string, string | number> = {
      bucketRaw: bucket,
      bucket: formatShanghaiBucket(bucket, data.bucket_unit),
    };

    for (const user of data.user_series) {
      const point = user.points.find((item) => item.bucket === bucket);
      row[user.username] = point ? metricValueFromPoint(point, metric) : 0;
    }

    return row;
  });
}

function bucketToUtcRange(
  bucket: string,
  bucketUnit: "hour" | "day",
): { startedFrom: string; startedTo: string } {
  if (bucketUnit === "hour") {
    const [datePart, hourPart] = bucket.split(" ");
    const hh = hourPart.slice(0, 2);
    const start = new Date(`${datePart}T${hh}:00:00+08:00`);
    const end = new Date(start.getTime() + 60 * 60 * 1000);
    return { startedFrom: start.toISOString(), startedTo: end.toISOString() };
  }

  const start = new Date(`${bucket}T00:00:00+08:00`);
  const end = new Date(start.getTime() + 24 * 60 * 60 * 1000);
  return { startedFrom: start.toISOString(), startedTo: end.toISOString() };
}
