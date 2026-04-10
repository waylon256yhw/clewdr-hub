const NANO = 1_000_000_000;

export function formatCost(nanousd: number): string {
  if (nanousd === 0) return "$0.00";
  const usd = nanousd / NANO;
  return usd < 0.01 ? `$${usd.toFixed(4)}` : `$${usd.toFixed(2)}`;
}

export function formatTokenCount(tokens: number): string {
  if (tokens === 0) return "0";
  if (Math.abs(tokens) >= 1_000_000_000) return `${(tokens / 1_000_000_000).toFixed(1)}B`;
  if (Math.abs(tokens) >= 1_000_000) return `${(tokens / 1_000_000).toFixed(1)}M`;
  if (Math.abs(tokens) >= 1_000) return `${(tokens / 1_000).toFixed(1)}K`;
  return tokens.toLocaleString("zh-CN");
}

export function formatCompactCount(value: number): string {
  return new Intl.NumberFormat("zh-CN", {
    notation: "compact",
    maximumFractionDigits: 1,
  }).format(value);
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
    case "invalid": case "upstream_error": case "auth_rejected": case "internal_error": return "red";
    case "exhausted": case "quota_rejected": return "yellow";
    default: return "blue";
  }
}

export function requestTypeColor(t: string): string {
  switch (t) {
    case "messages": return "blue";
    case "probe_cookie": return "grape";
    case "probe_oauth": return "teal";
    default: return "gray";
  }
}

export function formatShanghaiBucket(bucket: string, bucketUnit: "hour" | "day"): string {
  return bucketUnit === "hour" ? bucket.slice(5) : bucket.slice(5);
}
