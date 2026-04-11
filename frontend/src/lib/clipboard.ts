export type CopyMethod = "clipboard" | "execCommand";

export class CopyError extends Error {
  constructor(
    message: string,
    readonly code: "insecure-context" | "clipboard-unavailable" | "copy-failed",
    readonly cause?: unknown,
  ) {
    super(message);
    this.name = "CopyError";
  }
}

export async function copyText(text: string): Promise<CopyMethod> {
  const isSecureContext = typeof window !== "undefined" && window.isSecureContext;

  if (
    typeof navigator !== "undefined" &&
    typeof navigator.clipboard?.writeText === "function" &&
    isSecureContext
  ) {
    try {
      await navigator.clipboard.writeText(text);
      return "clipboard";
    } catch (error) {
      return fallbackCopyText(text, error);
    }
  }

  return fallbackCopyText(
    text,
    new CopyError(
      isSecureContext ? "Clipboard API 不可用" : "当前页面不是安全上下文",
      isSecureContext ? "clipboard-unavailable" : "insecure-context",
    ),
  );
}

function fallbackCopyText(text: string, cause?: unknown): CopyMethod {
  if (typeof document === "undefined" || !document.body) {
    throw normalizeCopyError(cause);
  }

  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.setAttribute("readonly", "");
  textarea.style.position = "fixed";
  textarea.style.opacity = "0";
  textarea.style.pointerEvents = "none";
  textarea.style.inset = "0";
  document.body.appendChild(textarea);

  try {
    textarea.focus();
    textarea.select();
    textarea.setSelectionRange(0, text.length);

    if (document.execCommand("copy")) {
      return "execCommand";
    }
  } catch (error) {
    throw new CopyError("浏览器拒绝复制", "copy-failed", error);
  } finally {
    document.body.removeChild(textarea);
  }

  throw normalizeCopyError(cause);
}

function normalizeCopyError(error: unknown): CopyError {
  if (error instanceof CopyError) {
    return error;
  }

  return new CopyError("浏览器拒绝复制", "copy-failed", error);
}

export function selectTextField(element: HTMLInputElement | HTMLTextAreaElement | null) {
  if (!element) return;

  element.focus();
  element.select();
  element.setSelectionRange(0, element.value.length);
}

export function getCopyFailureMessage(error: unknown) {
  if (error instanceof CopyError) {
    if (error.code === "insecure-context") {
      return "当前页面不是 HTTPS 或 localhost，浏览器可能拒绝复制。";
    }

    if (error.code === "clipboard-unavailable") {
      return "当前浏览器不支持直接写入剪贴板。";
    }
  }

  return "浏览器拒绝了复制请求。";
}
