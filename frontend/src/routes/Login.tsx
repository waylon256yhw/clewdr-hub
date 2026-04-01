import { useState } from "react";
import {
  Center,
  Paper,
  Title,
  TextInput,
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
  const { token, loading: authLoading, login } = useAuth();
  const navigate = useNavigate();
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [loading, setLoading] = useState(false);

  if (token && !authLoading) return <Navigate to="/" replace />;

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!username.trim() || !password) return;
    setLoading(true);
    try {
      await login(username.trim(), password);
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
            <Title order={3} ta="center">
              ClewdR 管理面板
            </Title>
            <TextInput
              label="用户名"
              placeholder="admin"
              value={username}
              onChange={(e) => setUsername(e.currentTarget.value)}
              required
              autoFocus
            />
            <PasswordInput
              label="密码"
              placeholder="请输入密码"
              value={password}
              onChange={(e) => setPassword(e.currentTarget.value)}
              required
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
