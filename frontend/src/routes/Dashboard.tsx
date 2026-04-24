import { useQuery } from "@tanstack/react-query";
import {
  SimpleGrid,
  Paper,
  Text,
  Group,
  Title,
  ActionIcon,
  Badge,
  Skeleton,
  Alert,
} from "@mantine/core";
import { IconRefresh } from "@tabler/icons-react";
import { useNavigate } from "react-router";
import { getOverview, qk } from "../api";

export default function Dashboard() {
  const navigate = useNavigate();
  const { data, isLoading, error, refetch } = useQuery({
    queryKey: qk.overview,
    queryFn: getOverview,
    refetchInterval: 30_000,
  });
  if (isLoading) {
    return (
      <>
        <Title order={3} mb="md">总览</Title>
        <SimpleGrid cols={{ base: 1, xs: 2, md: 3 }} spacing="md">
          {Array.from({ length: 6 }).map((_, i) => (
            <Skeleton key={i} height={100} radius="md" />
          ))}
        </SimpleGrid>
      </>
    );
  }

  if (error || !data) {
    return (
      <Alert color="red" title="加载失败">
        {String(error ?? "未知错误")}
        <br />
        <ActionIcon variant="light" mt="xs" onClick={() => refetch()}>
          <IconRefresh size={16} />
        </ActionIcon>
      </Alert>
    );
  }

  const cards: { label: string; value: React.ReactNode; link?: string }[] = [
    {
      label: "账号状态",
      link: "/accounts",
      value: (
        <Group gap="xs">
          <Badge color="green" variant="light">{data.accounts.statuses.active} active</Badge>
          <Badge color="yellow" variant="light">{data.accounts.statuses.cooling} cooling</Badge>
          <Badge color="red" variant="light">{data.accounts.statuses.error} error</Badge>
          <Badge color="gray" variant="light">{data.accounts.statuses.disabled} disabled</Badge>
        </Group>
      ),
    },
    {
      label: "调度细节",
      link: "/accounts",
      value: (
        <Group gap="xs">
          <Badge color="green" variant="light">
            {data.pool.detail.dispatchable_now} dispatchable
          </Badge>
          <Badge color="yellow" variant="light">
            {data.pool.detail.saturated} saturated
          </Badge>
          <Badge color="blue" variant="light">
            {data.pool.detail.probing} probing
          </Badge>
          <Badge color="gray" variant="light">
            {data.pool.detail.unconfigured} unconfigured
          </Badge>
        </Group>
      ),
    },
    {
      label: "认证结构",
      link: "/accounts",
      value: (
        <Group gap="xs">
          <Badge color="blue" variant="light">{data.accounts.auth_sources.oauth} OAuth</Badge>
          <Badge color="dark" variant="outline">{data.accounts.auth_sources.cookie} Cookie</Badge>
        </Group>
      ),
    },
    {
      label: "用户",
      link: "/users",
      value: (
        <Group gap="xs">
          <Text fw={600} size="xl">{data.users.total}</Text>
          <Text size="sm" c="dimmed">({data.users.admins} 管理员, {data.users.members} 成员)</Text>
        </Group>
      ),
    },
    {
      label: "API Key",
      link: "/keys",
      value: (
        <Group gap="xs">
          <Badge color="green" variant="light">{data.api_keys.active} 活跃</Badge>
          <Badge color="gray" variant="light">{data.api_keys.disabled} 禁用</Badge>
        </Group>
      ),
    },
    {
      label: "请求量",
      value: (
        <Group gap="xs">
          <Text fw={600} size="xl">{data.requests_1h}</Text>
          <Text size="sm" c="dimmed">/ 1小时</Text>
          <Text fw={600} size="xl">{data.requests_24h}</Text>
          <Text size="sm" c="dimmed">/ 24小时</Text>
        </Group>
      ),
    },
    {
      label: "伪装版本",
      value: (
        <Text size="sm">
          CLI {data.stealth.cli_version}
        </Text>
      ),
    },
  ];

  return (
    <>
      <Group justify="space-between" mb="md">
        <Title order={3}>总览</Title>
        <Text size="xs" c="dimmed">{data.version}</Text>
      </Group>
      <SimpleGrid cols={{ base: 1, xs: 2, md: 3 }} spacing="md">
        {cards.map((card) => (
          <Paper
            key={card.label}
            shadow="xs"
            p="md"
            radius="md"
            withBorder
            style={card.link ? { cursor: "pointer" } : undefined}
            onClick={card.link ? () => navigate(card.link!) : undefined}
          >
            <Text size="sm" c="dimmed" mb="xs">{card.label}</Text>
            {card.value}
          </Paper>
        ))}
      </SimpleGrid>
    </>
  );
}
