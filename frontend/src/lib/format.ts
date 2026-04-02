const NANO = 1_000_000_000;

export function formatCost(nanousd: number): string {
  if (nanousd === 0) return "$0.00";
  const usd = nanousd / NANO;
  return usd < 0.01 ? `$${usd.toFixed(4)}` : `$${usd.toFixed(2)}`;
}

export function formatDate(iso: string | null | undefined): string {
  if (!iso) return "—";
  return new Date(iso).toLocaleString("zh-CN", {
    timeZone: "Asia/Shanghai",
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
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
