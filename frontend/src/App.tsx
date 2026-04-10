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
  useMantineColorScheme,
  useComputedColorScheme,
} from "@mantine/core";
import { Notifications } from "@mantine/notifications";
import { useDisclosure } from "@mantine/hooks";
import { Routes, Route, Navigate, useLocation, Link } from "react-router";
import { Suspense, lazy, useEffect, useRef } from "react";
import { useQueryClient } from "@tanstack/react-query";
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
import { qk } from "./api";
import { RequireAuth, useAuth, ForceChangePasswordModal } from "./auth";
import Login from "./routes/Login";
import Dashboard from "./routes/Dashboard";
import Accounts from "./routes/Accounts";
import Users from "./routes/Users";
import Keys from "./routes/Keys";
import Settings from "./routes/Settings";
import Logs from "./routes/Logs";
const Ops = lazy(() => import("./routes/Ops"));

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

function AdminShell() {
  const location = useLocation();
  const [opened, { toggle, close }] = useDisclosure();
  const { logout } = useAuth();
  useGlobalAdminEvents();

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
              <Suspense fallback={null}>
                <Ops />
              </Suspense>
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
