import { useState } from "react";
import {
  Center,
  Paper,
  Title,
  PasswordInput,
  Button,
  Stack,
  Text,
} from "@mantine/core";
import { notifications } from "@mantine/notifications";
import { Navigate, useNavigate } from "react-router";
import { useAuth } from "../auth";
import { ApiError } from "../api";

export default function Login() {
  const { user, loading: authLoading, login } = useAuth();
  const navigate = useNavigate();
  const [password, setPassword] = useState("");
  const [loading, setLoading] = useState(false);

  if (user && !authLoading) return <Navigate to="/" replace />;

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!password) return;
    setLoading(true);
    try {
      await login("admin", password);
      navigate("/", { replace: true });
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : "登录失败";
      notifications.show({ title: "登录失败", message: msg, color: "red" });
    } finally {
      setLoading(false);
    }
  };

  return (
    <Center h="100vh" px="md">
      <Paper shadow="md" p="xl" maw={360} w="100%" radius="md" withBorder>
        <form onSubmit={handleSubmit}>
          <Stack>
            <Center>
              <img src="/logo.svg" alt="clewdr-hub" width={72} height={72} />
            </Center>
            <Title order={3} ta="center">
              clewdr-hub 管理面板
            </Title>
            <PasswordInput
              label="密码"
              placeholder="请输入密码"
              value={password}
              onChange={(e) => setPassword(e.currentTarget.value)}
              required
              autoFocus
            />
            <Button type="submit" fullWidth loading={loading}>
              登录
            </Button>
            <Text size="xs" c="dimmed" ta="center">
              仅管理员可登录
            </Text>
          </Stack>
        </form>
      </Paper>
    </Center>
  );
}
