import "@mantine/core/styles.css";
import "@mantine/charts/styles.css";
import "@mantine/notifications/styles.css";
import {
  MantineProvider,
  AppShell,
  NavLink,
  Title,
  Burger,
  Group,
  ActionIcon,
  Alert,
  Button,
  SimpleGrid,
  Skeleton,
  Stack,
  Text,
  useMantineColorScheme,
  useComputedColorScheme,
} from "@mantine/core";
import { Notifications } from "@mantine/notifications";
import { useDisclosure } from "@mantine/hooks";
import { Routes, Route, Navigate, useLocation, Link } from "react-router";
import { Component, Suspense, lazy, useEffect, useRef, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  IconDashboard,
  IconServer,
  IconUsers,
  IconKey,
  IconSettings,
  IconFileText,
  IconActivity,
  IconSun,
  IconMoon,
  IconLogout,
} from "@tabler/icons-react";
import { theme } from "./theme";
import { getOverview, qk } from "./api";
import {
  RequireAuth,
  useAuth,
  ForceChangePasswordModal,
  reloadIfFrontendOutdated,
} from "./auth";
import Login from "./routes/Login";
import Dashboard from "./routes/Dashboard";
import Accounts from "./routes/Accounts";
import Users from "./routes/Users";
import Keys from "./routes/Keys";
import Settings from "./routes/Settings";
import Logs from "./routes/Logs";
async function retryImport<T>(fn: () => Promise<T>, retries = 2): Promise<T> {
  try {
    return await fn();
  } catch (err) {
    if (retries <= 0) throw err;
    await new Promise((r) => setTimeout(r, 1000));
    return retryImport(fn, retries - 1);
  }
}

const Ops = lazy(() => retryImport(() => import("./routes/Ops")));

// Catches failures inside the Ops subtree — lazy chunk load errors AND render
// errors (e.g. recharts choking on mobile viewport). Named "Ops" not "Chunk"
// because the scope is broader than just code-splitting failures.
class OpsErrorBoundary extends Component<
  { children: ReactNode },
  { hasError: boolean }
> {
  state = { hasError: false };

  static getDerivedStateFromError(): { hasError: boolean } {
    return { hasError: true };
  }

  override componentDidCatch(error: Error, info: React.ErrorInfo) {
    // Log in production too — white-screen reports are useless without this.
    console.error("[OpsErrorBoundary] caught error:", error, info.componentStack);
  }

  override render() {
    if (this.state.hasError) {
      return (
        <Alert color="red" title="页面加载失败" variant="light">
          <Stack gap="xs" align="flex-start">
            <Text size="sm">资源加载失败，请刷新页面重试。</Text>
            <Button size="xs" onClick={() => window.location.reload()}>
              刷新页面
            </Button>
          </Stack>
        </Alert>
      );
    }
    return this.props.children;
  }
}

// NOTE: kept in sync manually with the in-component skeleton in Ops.tsx.
// Do NOT import from Ops.tsx — that would pull this into the lazy chunk,
// defeating the whole point of having a fallback during chunk loading.
function OpsSkeleton() {
  return (
    <>
      <Title order={3} mb="md">运维</Title>
      <SimpleGrid cols={{ base: 1, sm: 2, xl: 4 }} spacing="md">
        {Array.from({ length: 4 }).map((_, i) => (
          <Skeleton key={i} height={108} radius="md" />
        ))}
      </SimpleGrid>
      <SimpleGrid cols={{ base: 1, xl: 2 }} spacing="md" mt="md">
        <Skeleton height={360} radius="md" />
        <Skeleton height={360} radius="md" />
      </SimpleGrid>
    </>
  );
}

const NAV_ITEMS = [
  { label: "总览", path: "/", icon: IconDashboard },
  { label: "账号池", path: "/accounts", icon: IconServer },
  { label: "用户管理", path: "/users", icon: IconUsers },
  { label: "API 密钥", path: "/keys", icon: IconKey },
  { label: "设置", path: "/settings", icon: IconSettings },
  { label: "日志", path: "/logs", icon: IconFileText },
  { label: "运维", path: "/ops", icon: IconActivity },
];

function ColorSchemeToggle() {
  const { setColorScheme } = useMantineColorScheme();
  const computed = useComputedColorScheme("light");
  return (
    <ActionIcon
      variant="default"
      size="lg"
      onClick={() => setColorScheme(computed === "light" ? "dark" : "light")}
      aria-label="切换主题"
    >
      {computed === "light" ? <IconMoon size={18} /> : <IconSun size={18} />}
    </ActionIcon>
  );
}

/**
 * Subscribe to admin SSE events at the AppShell level so the connection stays
 * active across page navigations. Per-page hooks would tear down on unmount and
 * lose any events broadcast while the user was on a different tab — which was
 * the cause of "manual probe didn't show up in logs" reports.
 */
function useGlobalAdminEvents() {
  const queryClient = useQueryClient();
  const reconnectTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
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
            queryClient.invalidateQueries({ queryKey: ["opsUsage"] });
            queryClient.invalidateQueries({ queryKey: qk.overview });
          }
        } catch {
          queryClient.invalidateQueries({ queryKey: ["requests"] });
          queryClient.invalidateQueries({ queryKey: ["opsUsage"] });
          queryClient.invalidateQueries({ queryKey: qk.overview });
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
  }, [queryClient]);
}

function useFrontendVersionSync() {
  const { data } = useQuery({
    queryKey: qk.overview,
    queryFn: getOverview,
    staleTime: 30_000,
    refetchInterval: 30_000,
    refetchOnWindowFocus: true,
    refetchOnReconnect: true,
  });

  useEffect(() => {
    reloadIfFrontendOutdated(data?.version);
  }, [data?.version]);
}

function AdminShell() {
  const location = useLocation();
  const [opened, { toggle, close }] = useDisclosure();
  const { logout } = useAuth();
  useGlobalAdminEvents();
  useFrontendVersionSync();

  return (
    <AppShell
      header={{ height: 56 }}
      navbar={{ width: 220, breakpoint: "sm", collapsed: { mobile: !opened } }}
      padding="md"
    >
      <AppShell.Header>
        <Group h="100%" px="md" justify="space-between">
          <Group>
            <Burger opened={opened} onClick={toggle} hiddenFrom="sm" size="sm" />
            <Title order={4}>clewdr-hub</Title>
          </Group>
          <Group gap="xs">
            <ColorSchemeToggle />
            <ActionIcon variant="default" size="lg" onClick={logout} aria-label="退出登录">
              <IconLogout size={18} />
            </ActionIcon>
          </Group>
        </Group>
      </AppShell.Header>
      <AppShell.Navbar p="sm">
        {NAV_ITEMS.map((item) => (
          <NavLink
            key={item.path}
            label={item.label}
            leftSection={<item.icon size={18} />}
            active={item.path === "/" ? location.pathname === "/" : location.pathname.startsWith(item.path)}
            component={Link}
            to={item.path}
            onClick={close}
          />
        ))}
      </AppShell.Navbar>
      <AppShell.Main>
        <Routes>
          <Route path="/" element={<Dashboard />} />
          <Route path="/accounts" element={<Accounts />} />
          <Route path="/users" element={<Users />} />
          <Route path="/keys" element={<Keys />} />
          <Route path="/settings" element={<Settings />} />
          <Route path="/logs" element={<Logs />} />
          <Route
            path="/ops"
            element={(
              <OpsErrorBoundary>
                <Suspense fallback={<OpsSkeleton />}>
                  <Ops />
                </Suspense>
              </OpsErrorBoundary>
            )}
          />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </AppShell.Main>
      <ForceChangePasswordModal />
    </AppShell>
  );
}

export default function App() {
  return (
    <MantineProvider theme={theme} defaultColorScheme="auto">
      <Notifications position="top-right" />
      <Routes>
        <Route path="/login" element={<Login />} />
        <Route
          path="/*"
          element={
            <RequireAuth>
              <AdminShell />
            </RequireAuth>
          }
        />
      </Routes>
    </MantineProvider>
  );
}
