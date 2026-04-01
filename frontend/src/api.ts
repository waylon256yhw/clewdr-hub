// --- Error ---

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

// --- Fetch wrapper ---

interface FetchOptions {
  method?: string;
  body?: unknown;
  params?: Record<string, string | number | undefined>;
}

export async function apiFetch<T>(path: string, opts?: FetchOptions): Promise<T> {
  const url = new URL(path, window.location.origin);
  if (opts?.params) {
    for (const [k, v] of Object.entries(opts.params)) {
      if (v !== undefined) url.searchParams.set(k, String(v));
    }
  }

  const headers: Record<string, string> = {};
  if (opts?.body) headers["Content-Type"] = "application/json";

  const res = await fetch(url.toString(), {
    method: opts?.method ?? "GET",
    headers,
    credentials: "include",
    body: opts?.body ? JSON.stringify(opts.body) : undefined,
  });

  if (res.status === 401) {
    window.dispatchEvent(new Event("auth:logout"));
    throw new ApiError(401, "登录已过期");
  }

  if (!res.ok) {
    let msg = res.statusText;
    try {
      const body = await res.json();
      msg = body?.error?.message ?? body?.error ?? msg;
    } catch {}
    throw new ApiError(res.status, msg);
  }

  if (res.status === 204) return undefined as T;
  return res.json();
}

// --- Types ---

export interface Paginated<T> {
  items: T[];
  total: number;
  offset: number;
  limit: number;
}

export interface OverviewResponse {
  version: string;
  server_time: string;
  cookies: { valid: number; exhausted: number; invalid: number };
  users: { total: number; admins: number; members: number; disabled: number };
  api_keys: { total: number; active: number; disabled: number };
  accounts: { total: number; active: number; disabled: number };
  policies: number;
  requests_1h: number;
  requests_24h: number;
  stealth: { cli_version: string; sdk_version: string };
  must_change_password: boolean;
}

export interface Account {
  id: number;
  name: string;
  rr_order: number;
  max_slots: number;
  status: string;
  organization_uuid: string | null;
  invalid_reason: string | null;
  last_refresh_at: string | null;
  last_used_at: string | null;
  last_error: string | null;
  created_at: string;
  updated_at: string;
}

export interface UserRow {
  id: number;
  username: string;
  display_name: string | null;
  role: string;
  policy_id: number;
  policy_name: string;
  disabled_at: string | null;
  last_seen_at: string | null;
  notes: string | null;
  key_count: number;
  current_week_cost_nanousd: number;
  current_month_cost_nanousd: number;
  created_at: string;
  updated_at: string;
}

export interface Policy {
  id: number;
  name: string;
  max_concurrent: number;
  rpm_limit: number;
  weekly_budget_nanousd: number;
  monthly_budget_nanousd: number;
  assigned_user_count: number;
  created_at: string;
  updated_at: string;
}

export interface KeyRow {
  id: number;
  user_id: number;
  username: string;
  label: string | null;
  lookup_key: string;
  plaintext_key: string | null;
  disabled_at: string | null;
  expires_at: string | null;
  last_used_at: string | null;
  last_used_ip: string | null;
  created_at: string;
}

export interface KeyCreated {
  id: number;
  user_id: number;
  label: string | null;
  lookup_key: string;
  plaintext_key: string;
  created_at: string;
}

export interface RequestLog {
  id: number;
  request_id: string;
  request_type: string;
  user_id: number | null;
  username: string | null;
  api_key_id: number | null;
  key_label: string | null;
  account_id: number | null;
  account_name: string | null;
  model_raw: string;
  model_normalized: string | null;
  stream: number;
  started_at: string;
  completed_at: string | null;
  duration_ms: number | null;
  status: string;
  http_status: number | null;
  input_tokens: number | null;
  output_tokens: number | null;
  cost_nanousd: number;
  error_code: string | null;
  error_message: string | null;
}

export interface LoginResponse {
  user_id: number;
  username: string;
  role: string;
  must_change_password: boolean;
}

export interface CliVersionsResponse {
  versions: string[];
  cached: boolean;
  fetched_at: string | null;
}

// --- Endpoints ---

// Auth
export const login = (data: { username: string; password: string }) =>
  apiFetch<LoginResponse>("/auth/login", { method: "POST", body: data });
export const logout = () =>
  apiFetch<void>("/auth/logout", { method: "POST" }).catch(() => {});

// Overview
export const getOverview = () => apiFetch<OverviewResponse>("/api/admin/overview");

// Version (no auth)
export const getVersion = () => fetch("/api/version").then((r) => r.text());

// Accounts
export const listAccounts = () =>
  apiFetch<Paginated<Account>>("/api/admin/accounts", { params: { limit: 100 } });
export const createAccount = (data: {
  name: string;
  rr_order: number;
  max_slots?: number;
  cookie_blob: string;
  organization_uuid?: string;
}) => apiFetch<Account>("/api/admin/accounts", { method: "POST", body: data });
export const updateAccount = (id: number, data: Record<string, unknown>) =>
  apiFetch<Account>(`/api/admin/accounts/${id}`, { method: "PUT", body: data });
export const deleteAccount = (id: number) =>
  apiFetch<void>(`/api/admin/accounts/${id}`, { method: "DELETE" });

// Users
export const listUsers = () =>
  apiFetch<Paginated<UserRow>>("/api/admin/users", { params: { limit: 100 } });
export const createUser = (data: Record<string, unknown>) =>
  apiFetch<UserRow>("/api/admin/users", { method: "POST", body: data });
export const updateUser = (id: number, data: Record<string, unknown>) =>
  apiFetch<UserRow>(`/api/admin/users/${id}`, { method: "PUT", body: data });
export const deleteUser = (id: number) =>
  apiFetch<void>(`/api/admin/users/${id}`, { method: "DELETE" });

// Policies
export const listPolicies = () =>
  apiFetch<Paginated<Policy>>("/api/admin/policies", { params: { limit: 100 } });
export const createPolicy = (data: Record<string, unknown>) =>
  apiFetch<Policy>("/api/admin/policies", { method: "POST", body: data });
export const updatePolicy = (id: number, data: Record<string, unknown>) =>
  apiFetch<Policy>(`/api/admin/policies/${id}`, { method: "PUT", body: data });
export const deletePolicy = (id: number) =>
  apiFetch<void>(`/api/admin/policies/${id}`, { method: "DELETE" });

// Keys
export const listKeys = (userId?: number) =>
  apiFetch<Paginated<KeyRow>>("/api/admin/keys", {
    params: { limit: 100, ...(userId !== undefined ? { user_id: userId } : {}) },
  });
export const createKey = (data: { user_id: number; label?: string }) =>
  apiFetch<KeyCreated>("/api/admin/keys", { method: "POST", body: data });
export const deleteKey = (id: number) =>
  apiFetch<void>(`/api/admin/keys/${id}`, { method: "DELETE" });

// Settings
export const getSettings = () => apiFetch<Record<string, string>>("/api/admin/settings");
export const updateSettings = (settings: Record<string, string>) =>
  apiFetch<Record<string, string>>("/api/admin/settings", { method: "POST", body: { settings } });
export const getCliVersions = (force?: boolean) =>
  apiFetch<CliVersionsResponse>(`/api/admin/cli-versions${force ? "?force=1" : ""}`);

// Requests
export interface RequestFilters {
  offset?: number;
  limit?: number;
  user_id?: number;
  status?: string;
  model?: string;
  started_from?: string;
  started_to?: string;
}
export const listRequests = (filters: RequestFilters) =>
  apiFetch<Paginated<RequestLog>>("/api/admin/requests", {
    params: filters as Record<string, string | number | undefined>,
  });

// Me
export const changePassword = (data: { current_password: string; new_password: string }) =>
  apiFetch<{ message: string }>("/api/admin/me/password", { method: "PUT", body: data });

// --- Query Keys ---

export const qk = {
  overview: ["overview"] as const,
  accounts: ["accounts"] as const,
  users: ["users"] as const,
  policies: ["policies"] as const,
  keys: (userId?: number) => ["keys", userId] as const,
  settings: ["settings"] as const,
  requests: (filters: RequestFilters) => ["requests", filters] as const,
};
