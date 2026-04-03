import { createContext, use, useState, useEffect, useCallback, type ReactNode } from "react";
import { useNavigate, Navigate } from "react-router";
import { useQueryClient } from "@tanstack/react-query";
import {
  Modal,
  Stack,
  Text,
  PasswordInput,
  Button,
} from "@mantine/core";
import { useForm } from "@mantine/form";
import { notifications } from "@mantine/notifications";
import {
  login as apiLogin,
  logout as apiLogout,
  changePassword,
  getOverview,
  ApiError,
  type LoginResponse,
} from "./api";

interface AuthUser {
  user_id: number;
  username: string;
  role: string;
}

interface AuthContextValue {
  user: AuthUser | null;
  loading: boolean;
  mustChangePassword: boolean;
  setMustChangePassword: (v: boolean) => void;
  login: (username: string, password: string) => Promise<LoginResponse>;
  logout: () => void;
}

const AuthContext = createContext<AuthContextValue | null>(null);

export function useAuth() {
  const ctx = use(AuthContext);
  if (!ctx) throw new Error("useAuth must be inside AuthProvider");
  return ctx;
}

export function AuthProvider({ children }: { children: ReactNode }) {
  const [user, setUser] = useState<AuthUser | null>(null);
  const [loading, setLoading] = useState(true);
  const [mustChangePassword, setMustChangePassword] = useState(false);
  const queryClient = useQueryClient();
  const navigate = useNavigate();

  useEffect(() => {
    getOverview()
      .then((data) => {
        setUser({ user_id: 0, username: "admin", role: "admin" });
        if (data.must_change_password) {
          setMustChangePassword(true);
        }
      })
      .catch(() => {
        setUser(null);
      })
      .finally(() => setLoading(false));
  }, []);

  useEffect(() => {
    const handler = () => {
      setUser(null);
      setMustChangePassword(false);
      queryClient.clear();
      navigate("/login", { replace: true });
    };
    window.addEventListener("auth:logout", handler);
    return () => window.removeEventListener("auth:logout", handler);
  }, [queryClient, navigate]);

  const login = useCallback(async (username: string, password: string) => {
    const res = await apiLogin({ username, password });
    setUser({ user_id: res.user_id, username: res.username, role: res.role });
    if (res.must_change_password) {
      setMustChangePassword(true);
    }
    return res;
  }, []);

  const logout = useCallback(() => {
    apiLogout();
    setUser(null);
    setMustChangePassword(false);
    queryClient.clear();
    navigate("/login", { replace: true });
  }, [queryClient, navigate]);

  return (
    <AuthContext value={{ user, loading, mustChangePassword, setMustChangePassword, login, logout }}>
      {children}
    </AuthContext>
  );
}

export function RequireAuth({ children }: { children: ReactNode }) {
  const { user, loading } = useAuth();
  if (loading) return null;
  if (!user) return <Navigate to="/login" replace />;
  return children;
}

export function ForceChangePasswordModal() {
  const { mustChangePassword, setMustChangePassword } = useAuth();
  const [open, setOpen] = useState(false);
  const [submitting, setSubmitting] = useState(false);

  useEffect(() => {
    if (mustChangePassword) setOpen(true);
  }, [mustChangePassword]);

  const form = useForm({
    mode: "uncontrolled",
    initialValues: { current_password: "password", new_password: "", confirm: "" },
    validate: {
      new_password: (v) => (v.length < 6 ? "密码至少 6 个字符" : null),
      confirm: (v, values) => (v !== values.new_password ? "两次输入不一致" : null),
    },
  });

  const handleSubmit = async ({ current_password, new_password }: typeof form.values) => {
    setSubmitting(true);
    try {
      await changePassword({ current_password, new_password });
      setMustChangePassword(false);
      notifications.show({ message: "密码已修改，请牢记新密码", color: "green" });
      setOpen(false);
    } catch (err) {
      notifications.show({
        message: err instanceof ApiError ? err.message : "修改失败",
        color: "red",
      });
    } finally {
      setSubmitting(false);
    }
  };

  if (!open) return null;

  return (
    <Modal
      opened
      onClose={() => {}}
      withCloseButton={false}
      title="首次登录 — 请修改默认密码"
      closeOnEscape={false}
      closeOnClickOutside={false}
    >
      <form onSubmit={form.onSubmit(handleSubmit)}>
        <Stack>
          <Text size="sm" c="dimmed">
            您正在使用默认密码，为了安全请立即修改。
          </Text>
          <PasswordInput
            label="当前密码"
            key={form.key("current_password")}
            {...form.getInputProps("current_password")}
          />
          <PasswordInput
            label="新密码"
            placeholder="至少 6 个字符"
            key={form.key("new_password")}
            {...form.getInputProps("new_password")}
          />
          <PasswordInput
            label="确认新密码"
            key={form.key("confirm")}
            {...form.getInputProps("confirm")}
          />
          <Button type="submit" fullWidth loading={submitting}>
            修改密码
          </Button>
        </Stack>
      </form>
    </Modal>
  );
}
