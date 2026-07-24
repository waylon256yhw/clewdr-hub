import {
  createContext,
  use,
  useState,
  useEffect,
  useCallback,
  useMemo,
  useRef,
  type ReactNode,
} from "react";
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
  getSession,
  getOverview,
  qk,
  ApiError,
  type LoginResponse,
} from "./api";

const VERSION_RELOAD_KEY = "clewdr:frontend-version-reload";

export function reloadIfFrontendOutdated(serverVersion: string | undefined): boolean {
  if (!serverVersion) {
    return false;
  }

  if (serverVersion === __APP_VERSION__) {
    try {
      sessionStorage.removeItem(VERSION_RELOAD_KEY);
    } catch {}
    return false;
  }

  const reloadToken = `${__APP_VERSION__}->${serverVersion}`;
  try {
    if (sessionStorage.getItem(VERSION_RELOAD_KEY) === reloadToken) {
      return false;
    }
    sessionStorage.setItem(VERSION_RELOAD_KEY, reloadToken);
  } catch {}

  window.location.reload();
  return true;
}

interface AuthUser {
  user_id: number;
  username: string;
  role: string;
}

interface AuthContextValue {
  user: AuthUser | null;
  loading: boolean;
  expiresAt: number | null;
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
  const [expiresAt, setExpiresAt] = useState<number | null>(null);
  const [mustChangePassword, setMustChangePassword] = useState(false);
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const lastLogoutNoticeAt = useRef(0);

  useEffect(() => {
    getSession()
      .then(async (session) => {
        setUser({
          user_id: session.user_id,
          username: session.username,
          role: session.role,
        });
        setExpiresAt(session.expires_at);
        setMustChangePassword(session.must_change_password);
        if (!session.must_change_password) {
          const data = await queryClient.fetchQuery({
            queryKey: qk.overview,
            queryFn: getOverview,
            staleTime: 30_000,
          });
          reloadIfFrontendOutdated(data.version);
        }
      })
      .catch(() => {
        setUser(null);
      })
      .finally(() => setLoading(false));
  }, [queryClient]);

  useEffect(() => {
    const handler = (event: Event) => {
      setUser(null);
      setExpiresAt(null);
      setMustChangePassword(false);
      queryClient.clear();
      const message = (event as CustomEvent<{ message?: string }>).detail?.message;
      const now = Date.now();
      if (message && now - lastLogoutNoticeAt.current > 1_000) {
        lastLogoutNoticeAt.current = now;
        notifications.show({ message, color: "blue" });
      }
      navigate("/login", { replace: true });
    };
    window.addEventListener("auth:logout", handler);
    return () => window.removeEventListener("auth:logout", handler);
  }, [queryClient, navigate]);

  const login = useCallback(async (username: string, password: string) => {
    const res = await apiLogin({ username, password });
    setUser({ user_id: res.user_id, username: res.username, role: res.role });
    setExpiresAt(res.expires_at);
    setMustChangePassword(res.must_change_password);
    if (!res.must_change_password) {
      const overview = await queryClient.fetchQuery({
        queryKey: qk.overview,
        queryFn: getOverview,
        staleTime: 30_000,
      });
      reloadIfFrontendOutdated(overview.version);
    }
    return res;
  }, [queryClient]);

  const logout = useCallback(() => {
    apiLogout();
    setUser(null);
    setExpiresAt(null);
    setMustChangePassword(false);
    queryClient.clear();
    navigate("/login", { replace: true });
  }, [queryClient, navigate]);

  // Memoize the context value so consumers don't re-render on every
  // AuthProvider render — only when an actual auth field changes.
  // setMustChangePassword is a stable useState setter, so it's omitted from deps.
  const value = useMemo(
    () => ({
      user,
      loading,
      expiresAt,
      mustChangePassword,
      setMustChangePassword,
      login,
      logout,
    }),
    [user, loading, expiresAt, mustChangePassword, login, logout],
  );

  return <AuthContext value={value}>{children}</AuthContext>;
}

export function RequireAuth({ children }: { children: ReactNode }) {
  const { user, loading } = useAuth();
  if (loading) return null;
  if (!user) return <Navigate to="/login" replace />;
  return children;
}

export function ForceChangePasswordModal() {
  const { mustChangePassword } = useAuth();
  const [open, setOpen] = useState(false);
  const [submitting, setSubmitting] = useState(false);

  useEffect(() => {
    if (mustChangePassword) setOpen(true);
  }, [mustChangePassword]);

  const form = useForm({
    mode: "uncontrolled",
    initialValues: { current_password: "", new_password: "", confirm: "" },
    validate: {
      new_password: (v) => (v.length < 6 ? "密码至少 6 个字符" : null),
      confirm: (v, values) => (v !== values.new_password ? "两次输入不一致" : null),
    },
  });

  const handleSubmit = async ({ current_password, new_password }: typeof form.values) => {
    setSubmitting(true);
    try {
      await changePassword({ current_password, new_password });
      window.dispatchEvent(new CustomEvent("auth:logout", {
        detail: { message: "密码已修改，所有设备均已退出，请使用新密码重新登录" },
      }));
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
