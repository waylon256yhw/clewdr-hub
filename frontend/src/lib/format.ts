const NANO = 1_000_000_000;

export function formatCost(nanousd: number): string {
  if (nanousd === 0) return "$0.00";
  const usd = nanousd / NANO;
  return usd < 0.01 ? `$${usd.toFixed(4)}` : `$${usd.toFixed(2)}`;
}

export function formatDate(iso: string | null | undefined): string {
  if (!iso) return "—";
  const d = new Date(iso);
  const now = Date.now();
  const diff = now - d.getTime();
  if (diff < 60_000) return "刚刚";
  if (diff < 3600_000) return `${Math.floor(diff / 60_000)} 分钟前`;
  if (diff < 86400_000) return `${Math.floor(diff / 3600_000)} 小时前`;
  if (diff < 604800_000) return `${Math.floor(diff / 86400_000)} 天前`;
  return d.toLocaleDateString("zh-CN", { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

export function statusColor(status: string): string {
  switch (status) {
    case "active": case "ok": return "green";
    case "disabled": return "gray";
    case "invalid": case "upstream_error": case "auth_rejected": return "red";
    case "exhausted": case "quota_rejected": return "yellow";
    default: return "blue";
  }
}
