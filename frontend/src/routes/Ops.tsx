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
  Tooltip,
} from "@mantine/core";
import { useNavigate } from "react-router";
import {
  getOpsUsage,
  listUsers,
  qk,
  type OpsDimensionItem,
  type OpsMetric,
  type OpsRange,
  type OpsSeries,
  type OpsSeriesPoint,
  type OpsUsageResponse,
  type OpsUsageTotals,
} from "../api";
import {
  formatCompactCount,
  formatCost,
  formatDate,
  formatShanghaiBucket,
  formatTokenCount,
} from "../lib/format";

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

// ---- Metric helpers -------------------------------------------------------

function metricValueFromTotals(totals: OpsUsageTotals, metric: OpsMetric): number {
  if (metric === "cost") return totals.cost_nanousd / 1_000_000_000;
  if (metric === "tokens") return totals.total_tokens;
  return totals.request_count;
}

function metricValueFromItem(item: OpsDimensionItem, metric: OpsMetric): number {
  if (metric === "cost") return item.cost_nanousd / 1_000_000_000;
  if (metric === "tokens") return item.total_tokens;
  return item.request_count;
}

function metricValueFromPoint(point: OpsSeriesPoint, metric: OpsMetric): number {
  if (metric === "cost") return point.cost_nanousd / 1_000_000_000;
  if (metric === "tokens") return point.total_tokens;
  return point.request_count;
}

function formatMetric(metric: OpsMetric, value: number): string {
  if (metric === "cost") return `$${value.toFixed(value < 0.01 ? 4 : 2)}`;
  if (metric === "tokens") return formatTokenCount(Math.round(value));
  return formatCompactCount(Math.round(value));
}

function metricLabel(metric: OpsMetric): string {
  if (metric === "cost") return "金额";
  if (metric === "tokens") return "Token";
  return "请求数";
}

function rangeLabel(range: OpsRange): string {
  if (range === "24h") return "近 24 小时";
  if (range === "7d") return "近 7 天";
  return "近 30 天";
}

// ---- Comparison helpers ---------------------------------------------------

interface ComparisonRender {
  /** Display string: "+12.3%", "-5.6%", "—" when null, "新增" when prev was 0 but current > 0. */
  text: string;
  /** "up" if ratio > 1, "down" if < 1, "flat" if = 1, "new" / "none" otherwise. */
  trend: "up" | "down" | "flat" | "new" | "none";
}

function renderComparisonRatio(
  ratio: number | null,
  windowTotalIsZero: boolean,
): ComparisonRender {
  if (ratio === null) {
    // null ratio means previous window had no data. If the current
    // window also has nothing, there is genuinely nothing to compare;
    // otherwise this is brand-new activity worth flagging as such.
    if (windowTotalIsZero) return { text: "—", trend: "none" };
    return { text: "新增", trend: "new" };
  }
  if (!Number.isFinite(ratio)) return { text: "—", trend: "none" };
  const delta = ratio - 1;
  const pct = Math.abs(delta) * 100;
  const sign = delta >= 0 ? "+" : "-";
  if (Math.abs(delta) < 0.0005) return { text: "0.0%", trend: "flat" };
  return {
    text: `${sign}${pct < 10 ? pct.toFixed(1) : pct.toFixed(0)}%`,
    trend: delta > 0 ? "up" : "down",
  };
}

function comparisonRatioForMetric(
  metric: OpsMetric,
  data: OpsUsageResponse,
): number | null {
  if (metric === "cost") return data.comparison.cost_ratio;
  if (metric === "tokens") return data.comparison.total_tokens_ratio;
  return data.comparison.request_count_ratio;
}

function trendColor(trend: ComparisonRender["trend"]): string {
  switch (trend) {
    case "up":
      // 增长视作"成本/用量上升"——给出引人注意的红色，
      // 与现网告警习惯一致；不区分 metric。
      return "red";
    case "down":
      return "teal";
    case "new":
      return "blue";
    case "flat":
    case "none":
    default:
      return "gray";
  }
}

// ---- KPI cards ------------------------------------------------------------

function KpiCard({
  label,
  value,
  hint,
  accent,
}: {
  label: string;
  value: React.ReactNode;
  hint?: React.ReactNode;
  accent?: string;
}) {
  return (
    <Paper shadow="xs" p="md" radius="md" withBorder>
      <Text size="sm" c="dimmed" mb={6}>{label}</Text>
      <Text fw={700} size="xl" c={accent}>{value}</Text>
      {hint ? <Text size="xs" c="dimmed" mt={4}>{hint}</Text> : null}
    </Paper>
  );
}

// ---- Main component -------------------------------------------------------

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
    queryKey: qk.opsUsage(range, metric, topUsersValue, selectedUserId),
    queryFn: () => getOpsUsage(range, metric, topUsersValue, selectedUserId),
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
        <Skeleton height={56} radius="md" mt="md" />
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

  const isUserSelected = selectedUserId != null;
  const dimensionLabel = data.dimension === "model" ? "模型" : "用户";

  const toLogs = (input: {
    startedFrom: string;
    startedTo: string;
    userId?: number;
    modelKey?: string;
  }) => {
    const search = new URLSearchParams();
    search.set("request_type", "messages");
    search.set("started_from", input.startedFrom);
    search.set("started_to", input.startedTo);
    if (input.userId != null) search.set("user_id", String(input.userId));
    // PR-B exposes an exact `model_key` filter so the donut slice for
    // "claude-opus-4-7" doesn't accidentally pull in
    // "claude-opus-4-7-experimental" via the legacy LIKE filter.
    if (input.modelKey) search.set("model_key", input.modelKey);
    navigate(`/logs?${search.toString()}`);
  };

  // ---- KPI: window totals + comparison ----
  const windowTotals = data.window_totals;
  const ratio = comparisonRatioForMetric(metric, data);
  const windowMetricZero = metricValueFromTotals(windowTotals, metric) === 0;
  const comparisonRender = renderComparisonRatio(ratio, windowMetricZero);
  const comparisonHint = data.comparison.complete
    ? `对比上一${range === "24h" ? "24 小时" : range === "7d" ? "7 天" : "30 天"}（完整桶）`
    : "数据积累中";

  // ---- Donut + line data ----
  const donutData = data.distribution.map((item, index) => ({
    name: item.label,
    value: metricValueFromItem(item, metric),
    color: CHART_COLORS[index % CHART_COLORS.length],
  }));
  const distributionLookup = new Map(
    data.distribution.map((item) => [item.label, item]),
  );

  const lineData = buildLineData(data, metric);
  const lineSeries = data.series.map((item, index) => ({
    name: item.label,
    color: CHART_COLORS[index % CHART_COLORS.length],
  }));
  const labelToSubject = new Map<string, OpsSeries>(
    data.series.map((item) => [item.label, item]),
  );

  // Reference line at the first partial bucket, if any — Recharts draws
  // a vertical line at that x value so the eye can tell "everything to
  // the right is still accumulating".
  const partialBucketKey = data.bucket_labels.find((b) => b.partial)?.key;
  const referenceLines = partialBucketKey
    ? [
        {
          x: partialBucketKey,
          label: "进行中",
          color: "gray.4",
          labelPosition: "insideTopRight" as const,
        },
      ]
    : [];

  // ---- Metadata bar pieces ----
  const windowLabel = data.comparison.window_label;
  const coverageFromBackfill = data.coverage.backfill_available_from;
  const coverageFromWrites = data.coverage.writes_started_at;
  const coverageBits: string[] = [];
  coverageBits.push(`窗口 ${windowLabel}`);
  coverageBits.push("已产生可计费用量的 messages 请求");
  if (range === "24h" && data.coverage.logs_available_from) {
    coverageBits.push(`日志可追溯至 ${formatDate(data.coverage.logs_available_from)}`);
  } else if (range !== "24h") {
    if (coverageFromWrites) {
      coverageBits.push(`持续聚合自 ${formatDate(coverageFromWrites)}`);
    }
    if (coverageFromBackfill) {
      coverageBits.push(`历史回填最早至 ${coverageFromBackfill}`);
    }
  }
  const accumulatingNotice =
    range === "30d" && !data.coverage.complete
      ? "30 天数据仍在积累"
      : range === "7d" && !data.coverage.complete
        ? "7 天数据仍在积累"
        : null;

  // ---- Lifetime Paper text ----
  const lifetime = data.lifetime_totals;

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
        <Tooltip
          label="已筛选单用户，折线和排行显示该用户的模型"
          disabled={!isUserSelected}
          withArrow
        >
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
            disabled={isUserSelected}
            w={TOP_USERS_FILTER_WIDTH}
          />
        </Tooltip>
      </Group>

      <Paper p="sm" radius="md" withBorder mb="md" bg="var(--mantine-color-gray-light)">
        <Text size="xs" c="dimmed">
          {coverageBits.join(" · ")}
          {accumulatingNotice ? (
            <Text component="span" c="orange" ml={6} fw={500}>· {accumulatingNotice}</Text>
          ) : null}
        </Text>
      </Paper>

      <SimpleGrid cols={{ base: 1, sm: 2, xl: 4 }} spacing="md" mb="md">
        <KpiCard
          label={`窗口请求数（${rangeLabel(range)}）`}
          value={formatCompactCount(windowTotals.request_count)}
        />
        <KpiCard
          label={`窗口 Token（${rangeLabel(range)}）`}
          value={formatTokenCount(windowTotals.total_tokens)}
          hint={(
            <Group gap={4} wrap="nowrap" mt={4}>
              <Badge size="sm" variant="light" color="cyan" radius="sm" title="输入 token">
                ↑{formatTokenCount(windowTotals.input_tokens)}
              </Badge>
              <Badge size="sm" variant="light" color="teal" radius="sm" title="输出 token">
                ↓{formatTokenCount(windowTotals.output_tokens)}
              </Badge>
              <Badge size="sm" variant="light" color="grape" radius="sm" title="缓存写入 (1.25× 输入价)">
                +{formatTokenCount(windowTotals.cache_creation_tokens)}
              </Badge>
              <Badge size="sm" variant="light" color="gray" radius="sm" title="缓存读取 (0.10× 输入价)">
                ↻{formatTokenCount(windowTotals.cache_read_tokens)}
              </Badge>
            </Group>
          )}
        />
        <KpiCard
          label={`窗口金额（${rangeLabel(range)}）`}
          value={formatCost(windowTotals.cost_nanousd)}
        />
        <KpiCard
          label={`${metricLabel(metric)} · 环比`}
          value={comparisonRender.text}
          accent={trendColor(comparisonRender.trend)}
          hint={comparisonHint}
        />
      </SimpleGrid>

      <Paper p="sm" radius="md" withBorder mb="md">
        <Group justify="space-between" align="start" wrap="wrap" gap="md">
          <Group gap="lg" wrap="wrap">
            <Group gap={6}>
              <Text size="xs" c="dimmed">全期累计 · 请求</Text>
              <Text size="sm" fw={600}>{formatCompactCount(lifetime.request_count)}</Text>
            </Group>
            <Group gap={6}>
              <Text size="xs" c="dimmed">Token</Text>
              <Text size="sm" fw={600}>{formatTokenCount(lifetime.total_tokens)}</Text>
            </Group>
            <Group gap={6}>
              <Text size="xs" c="dimmed">金额</Text>
              <Text size="sm" fw={600}>{formatCost(lifetime.cost_nanousd)}</Text>
            </Group>
          </Group>
          <Text size="xs" c="dimmed">
            累计含迁移前历史，可能与窗口数据口径略有差异
          </Text>
        </Group>
      </Paper>

      <SimpleGrid cols={{ base: 1, xl: 2 }} spacing="md" mb="md">
        <Paper shadow="xs" p="md" radius="md" withBorder>
          <Stack gap="xs">
            <Group justify="space-between">
              <Text fw={600}>模型分布</Text>
              <Badge variant="light">按{metricLabel(metric)}</Badge>
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
                cellProps={(series) => {
                  const item = distributionLookup.get(String(series.name));
                  const isOther = item?.is_other_bucket ?? false;
                  return {
                    style: { cursor: isOther ? "default" : "pointer", opacity: isOther ? 0.55 : 1 },
                    onClick: isOther
                      ? undefined
                      : () =>
                          toLogs({
                            startedFrom: data.window_started_at,
                            startedTo: data.window_ended_at,
                            userId: selectedUserId,
                            modelKey: item?.model_key ?? undefined,
                          }),
                  };
                }}
              />
            )}
          </Stack>
        </Paper>

        <Paper shadow="xs" p="md" radius="md" withBorder>
          <Stack gap="xs">
            <Group justify="space-between">
              <Text fw={600}>
                {isUserSelected ? "该用户各模型用量趋势" : "各用户用量跟踪"}
              </Text>
              <Badge variant="light">
                {isUserSelected ? `按${metricLabel(metric)} · 模型` : `Top ${topUsersValue} · 按${metricLabel(metric)}`}
              </Badge>
            </Group>
            {lineSeries.length === 0 ? (
              <Text c="dimmed" size="sm">当前窗口没有使用数据。</Text>
            ) : (
              <LineChart
                h={320}
                data={lineData}
                series={lineSeries}
                curveType="linear"
                withLegend
                dataKey="bucketRaw"
                referenceLines={referenceLines}
                xAxisProps={{
                  tickFormatter: (value) =>
                    formatShanghaiBucket(String(value), data.bucket_unit),
                }}
                lineChartProps={{
                  onClick: (state) => {
                    if (!state.activeLabel || !state.activeDataKey) return;
                    const bucket = String(state.activeLabel);
                    const label = String(state.activeDataKey);
                    const subject = labelToSubject.get(label);
                    if (!subject) return;
                    const { startedFrom, startedTo } = bucketToUtcRange(
                      bucket,
                      data.bucket_unit,
                    );
                    toLogs({
                      startedFrom,
                      startedTo,
                      userId: subject.user_id ?? selectedUserId,
                      modelKey: subject.model_key ?? undefined,
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
          <Group justify="space-between">
            <Text fw={600}>{isUserSelected ? "模型排行" : "用户排行"}</Text>
            <Badge variant="light">按{metricLabel(metric)}</Badge>
          </Group>
          {data.ranking.length === 0 ? (
            <Text c="dimmed" size="sm">当前窗口没有排行数据。</Text>
          ) : (
            <Table striped highlightOnHover>
              <Table.Thead>
                <Table.Tr>
                  <Table.Th>{dimensionLabel}</Table.Th>
                  <RankingHeadCell label="请求数" active={metric === "requests"} />
                  <RankingHeadCell label="总 Token" active={metric === "tokens"} />
                  <RankingHeadCell label="金额" active={metric === "cost"} />
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {data.ranking.map((item, idx) => {
                  const clickable = !item.is_other_bucket;
                  return (
                    <Table.Tr
                      key={`${item.kind}:${item.user_id ?? item.model_key ?? idx}`}
                      style={{
                        cursor: clickable ? "pointer" : "default",
                        opacity: clickable ? 1 : 0.7,
                      }}
                      onClick={
                        clickable
                          ? () =>
                              toLogs({
                                startedFrom: data.window_started_at,
                                startedTo: data.window_ended_at,
                                userId: item.user_id ?? selectedUserId,
                                modelKey: item.model_key ?? undefined,
                              })
                          : undefined
                      }
                    >
                      <Table.Td>
                        {item.is_other_bucket ? (
                          <Text size="sm" c="dimmed">{item.label}</Text>
                        ) : (
                          item.label
                        )}
                      </Table.Td>
                      <RankingCell value={item.request_count.toLocaleString("zh-CN")} active={metric === "requests"} />
                      <RankingCell value={formatTokenCount(item.total_tokens)} active={metric === "tokens"} />
                      <RankingCell value={formatCost(item.cost_nanousd)} active={metric === "cost"} />
                    </Table.Tr>
                  );
                })}
              </Table.Tbody>
            </Table>
          )}
        </Stack>
      </Paper>
    </>
  );
}

function RankingHeadCell({ label, active }: { label: string; active: boolean }) {
  return (
    <Table.Th style={{ fontWeight: active ? 700 : 500 }}>
      {label}
      {active ? <Badge size="xs" variant="light" ml={6}>当前</Badge> : null}
    </Table.Th>
  );
}

function RankingCell({ value, active }: { value: string; active: boolean }) {
  return (
    <Table.Td>
      <Text size="sm" fw={active ? 700 : 400}>{value}</Text>
    </Table.Td>
  );
}

function buildLineData(
  data: OpsUsageResponse,
  metric: OpsMetric,
): Record<string, string | number>[] {
  return data.bucket_labels.map((bucket) => {
    const row: Record<string, string | number> = {
      bucketRaw: bucket.key,
      bucket: formatShanghaiBucket(bucket.key, data.bucket_unit),
    };
    for (const subject of data.series) {
      const point = subject.points.find((item) => item.bucket === bucket.key);
      row[subject.label] = point ? metricValueFromPoint(point, metric) : 0;
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
