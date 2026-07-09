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
  Badge,
  Button,
  SimpleGrid,
  Skeleton,
  Stack,
  Text,
  useMantineColorScheme,
  useComputedColorScheme,
} from "@mantine/core";
import { Notifications } from "@mantine/notifications";
import { useDisclosure, useMediaQuery } from "@mantine/hooks";
import { Routes, Route, Navigate, useLocation, Link } from "react-router";
import { Component, Suspense, lazy, useEffect, useRef, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  IconDashboard,
  IconServer,
  IconPlug,
  IconUsers,
  IconKey,
  IconSettings,
  IconFileText,
  IconActivity,
  IconSun,
  IconMoon,
  IconBrandGithub,
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
async function retryImport<T>(fn: () => Promise<T>, retries = 2): Promise<T> {
  try {
    return await fn();
  } catch (err) {
    if (retries <= 0) throw err;
    await new Promise((r) => setTimeout(r, 1000));
    return retryImport(fn, retries - 1);
  }
}

// Every admin route is lazy so the initial bundle carries only the shell +
// Login. Route chunks load on first navigation; retryImport absorbs
// transient chunk-load failures (e.g. a deploy swapping hashed assets).
const Dashboard = lazy(() => retryImport(() => import("./routes/Dashboard")));
const Accounts = lazy(() => retryImport(() => import("./routes/Accounts")));
const Proxies = lazy(() => retryImport(() => import("./routes/Proxies")));
const Users = lazy(() => retryImport(() => import("./routes/Users")));
const Keys = lazy(() => retryImport(() => import("./routes/Keys")));
const Settings = lazy(() => retryImport(() => import("./routes/Settings")));
const Logs = lazy(() => retryImport(() => import("./routes/Logs")));
const Ops = lazy(() => retryImport(() => import("./routes/Ops")));

// Catches failures inside the routed subtree — lazy chunk load errors AND
// render errors (e.g. recharts choking on mobile viewport). Scope is broader
// than just code-splitting failures.
class RouteErrorBoundary extends Component<
  { children: ReactNode },
  { hasError: boolean }
> {
  state = { hasError: false };

  static getDerivedStateFromError(): { hasError: boolean } {
    return { hasError: true };
  }

  override componentDidCatch(error: Error, info: React.ErrorInfo) {
    // Log in production too — white-screen reports are useless without this.
    console.error("[RouteErrorBoundary] caught error:", error, info.componentStack);
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

// Generic fallback while a lazy route chunk loads. Deliberately sparse —
// route chunks are small and cached after first navigation.
function PageSkeleton() {
  return (
    <Stack gap="md">
      <Skeleton height={28} width={160} radius="sm" />
      <SimpleGrid cols={{ base: 1, sm: 2, xl: 4 }} spacing="md">
        {Array.from({ length: 4 }).map((_, i) => (
          <Skeleton key={i} height={108} radius="md" />
        ))}
      </SimpleGrid>
      <Skeleton height={320} radius="md" />
    </Stack>
  );
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
  { label: "账号", path: "/accounts", icon: IconServer },
  { label: "代理", path: "/proxies", icon: IconPlug },
  { label: "用户", path: "/users", icon: IconUsers },
  { label: "API", path: "/keys", icon: IconKey },
  { label: "设置", path: "/settings", icon: IconSettings },
  { label: "日志", path: "/logs", icon: IconFileText },
  { label: "运维", path: "/ops", icon: IconActivity },
];

function ColorSchemeToggle({ size = "lg", iconSize = 18 }: { size?: "md" | "lg"; iconSize?: number }) {
  const { setColorScheme } = useMantineColorScheme();
  const computed = useComputedColorScheme("light");
  return (
    <ActionIcon
      variant="default"
      size={size}
      onClick={() => setColorScheme(computed === "light" ? "dark" : "light")}
      aria-label="切换主题"
    >
      {computed === "light" ? <IconMoon size={iconSize} /> : <IconSun size={iconSize} />}
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
            queryClient.invalidateQueries({ queryKey: qk.requestsRoot });
            queryClient.invalidateQueries({ queryKey: qk.opsUsageRoot });
            queryClient.invalidateQueries({ queryKey: qk.overview });
            queryClient.invalidateQueries({ queryKey: qk.accounts });
          }
          if (payload.topic === "accounts") {
            queryClient.invalidateQueries({ queryKey: qk.accounts });
            queryClient.invalidateQueries({ queryKey: qk.overview });
          }
          if (payload.topic === "users") {
            queryClient.invalidateQueries({ queryKey: qk.users });
            queryClient.invalidateQueries({ queryKey: qk.overview });
          }
        } catch {
          queryClient.invalidateQueries({ queryKey: qk.requestsRoot });
          queryClient.invalidateQueries({ queryKey: qk.opsUsageRoot });
          queryClient.invalidateQueries({ queryKey: qk.overview });
          queryClient.invalidateQueries({ queryKey: qk.accounts });
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

  return data?.version;
}

function AdminShell() {
  const location = useLocation();
  const [opened, { toggle, close }] = useDisclosure();
  const { logout } = useAuth();
  useGlobalAdminEvents();
  const version = useFrontendVersionSync();
  const compactHeader = useMediaQuery("(max-width: 36em)");
  const headerActionSize = compactHeader ? "md" : "lg";
  const headerIconSize = compactHeader ? 17 : 18;
  const logoSize = compactHeader ? 26 : 28;

  return (
    <AppShell
      header={{ height: 56 }}
      navbar={{ width: 220, breakpoint: "sm", collapsed: { mobile: !opened } }}
      padding="md"
    >
      <AppShell.Header>
        <Group
          h="100%"
          px={compactHeader ? "xs" : "md"}
          gap={compactHeader ? 6 : "md"}
          justify="space-between"
          wrap="nowrap"
        >
          <Group gap="xs" wrap="nowrap" style={{ minWidth: 0, flex: "1 1 auto", overflow: "hidden" }}>
            <Burger opened={opened} onClick={toggle} hiddenFrom="sm" size="sm" />
            <img src="/logo.svg" alt="" width={logoSize} height={logoSize} style={{ flexShrink: 0 }} />
            <Title order={4} visibleFrom="xs">clewdr-hub</Title>
            {version && (
              <Badge
                size="sm"
                radius="sm"
                variant="gradient"
                gradient={{ from: "cyan", to: "blue" }}
                style={{ flexShrink: 0 }}
              >
                {version}
              </Badge>
            )}
          </Group>
          <Group gap={compactHeader ? 4 : "xs"} wrap="nowrap" style={{ flex: "0 0 auto" }}>
            <ColorSchemeToggle size={headerActionSize} iconSize={headerIconSize} />
            <ActionIcon
              component="a"
              href="https://github.com/waylon256yhw/clewdr-hub"
              target="_blank"
              rel="noreferrer"
              variant="default"
              size={headerActionSize}
              aria-label="打开 GitHub 仓库"
            >
              <IconBrandGithub size={headerIconSize} />
            </ActionIcon>
            <ActionIcon variant="default" size={headerActionSize} onClick={logout} aria-label="退出登录">
              <IconLogout size={headerIconSize} />
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
        <RouteErrorBoundary>
          <Suspense fallback={<PageSkeleton />}>
            <Routes>
              <Route path="/" element={<Dashboard />} />
              <Route path="/accounts" element={<Accounts />} />
              <Route path="/proxies" element={<Proxies />} />
              <Route path="/users" element={<Users />} />
              <Route path="/keys" element={<Keys />} />
              <Route path="/settings" element={<Settings />} />
              <Route path="/logs" element={<Logs />} />
              <Route
                path="/ops"
                element={(
                  <Suspense fallback={<OpsSkeleton />}>
                    <Ops />
                  </Suspense>
                )}
              />
              <Route path="*" element={<Navigate to="/" replace />} />
            </Routes>
          </Suspense>
        </RouteErrorBoundary>
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
