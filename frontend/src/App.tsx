import "@mantine/core/styles.css";
import { MantineProvider, AppShell, NavLink, Title, Burger, Group } from "@mantine/core";
import { useDisclosure } from "@mantine/hooks";
import { Routes, Route, Navigate, useLocation, Link } from "react-router";
import { theme } from "./theme";
import Dashboard from "./routes/Dashboard";
import Accounts from "./routes/Accounts";
import Users from "./routes/Users";
import Settings from "./routes/Settings";
import Logs from "./routes/Logs";

const NAV_ITEMS = [
  { label: "Overview", path: "/" },
  { label: "Accounts", path: "/accounts" },
  { label: "Users", path: "/users" },
  { label: "Settings", path: "/settings" },
  { label: "Logs", path: "/logs" },
];

export default function App() {
  const location = useLocation();
  const [opened, { toggle, close }] = useDisclosure();

  return (
    <MantineProvider theme={theme} defaultColorScheme="auto">
      <AppShell
        header={{ height: 50 }}
        navbar={{ width: 220, breakpoint: "sm", collapsed: { mobile: !opened } }}
        padding="md"
      >
        <AppShell.Header>
          <Group h="100%" px="md">
            <Burger opened={opened} onClick={toggle} hiddenFrom="sm" size="sm" />
            <Title order={4}>ClewdR</Title>
          </Group>
        </AppShell.Header>
        <AppShell.Navbar p="sm">
          {NAV_ITEMS.map((item) => (
            <NavLink
              key={item.path}
              label={item.label}
              active={location.pathname === item.path}
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
            <Route path="/settings" element={<Settings />} />
            <Route path="/logs" element={<Logs />} />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Routes>
        </AppShell.Main>
      </AppShell>
    </MantineProvider>
  );
}
