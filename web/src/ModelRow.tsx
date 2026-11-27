import { useEffect, useState } from "react";
import { EuiButton, EuiButtonEmpty } from "@elastic/eui";
import { api, message } from "./api";
import { getLocale, t } from "./locale";
import { ModelUpload } from "./ModelUpload";
import { Failure } from "./shared";

type Status = "missing" | "downloading" | "ready" | "failed";
export interface ModelFile {
  id: string;
  label: string;
  status: Status;
  downloaded_bytes: number;
  total_bytes: number | null;
  error?: string | null;
  files: string[];
  sources?: string[];
}
export const statuses: Record<Status, string> = {
  missing: "未下载", downloading: "下载中", ready: "已就绪", failed: "下载失败",
};

function size(bytes: number) {
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let quantity = Math.max(0, bytes);
  let unit = 0;
  while (quantity >= 1024 && unit < units.length - 1) { quantity /= 1024; unit++; }
  return `${quantity.toLocaleString(getLocale(), { maximumFractionDigits: 1 })} ${units[unit]}`;
}

function DownloadProgress({ model }: { model: ModelFile }) {
  const received = model.downloaded_bytes;
  const total = model.total_bytes;
  const known = total != null && total > 0;
  return <div className="model-progress">
    <progress aria-label={t("{0} 下载进度", model.label)} max={known ? total : undefined} value={known ? Math.min(received, total) : undefined} />
    <small>{known ? t("已下载 {0} / {1}", size(received), size(total)) : t("已下载 {0}", size(received))}</small>
  </div>;
}

export function ModelRow({ file, admin, active, source, refreshing, refresh }: {
  file: ModelFile; admin: boolean; active: boolean; source: string; refreshing: boolean; refresh: () => void;
}) {
  const [deleting, setDeleting] = useState(false);
  const [working, setWorking] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [uploading, setUploading] = useState(false);
  const busy = working || uploading || refreshing;
  const required = file.files ?? [];

  useEffect(() => { setError(""); setDeleting(false); }, [file.status]);

  async function change(action: "download" | "delete") {
    if (busy || !admin || (action === "delete" && active)) return;
    setWorking(true); setError(""); setNotice("");
    try {
      await api(`models/${action}`, action === "download" ? { id: file.id, source } : { id: file.id });
      setDeleting(false); setNotice(action === "download" ? "已提交模型下载任务。" : "模型文件已删除。"); refresh();
    } catch (e) { setError(message(e)); } finally { setWorking(false); }
  }

  return <li>
        <div className="model-file-header"><div><strong>{file.label}</strong>
          {file.label !== file.id && <small>{file.id}</small>}
          {file.status !== "downloading" && file.total_bytes != null && file.total_bytes > 0 && <small>{t("模型大小 {0}", size(file.total_bytes))}</small>}</div>
          <span className="model-state" data-status={file.status}>{t(active && file.status === "ready" ? "当前检索模型" : statuses[file.status])}</span></div>
        {file.status === "downloading" && <DownloadProgress model={file} />}
        {file.status === "failed" && file.error && <p className="model-error">{t(file.error)}</p>}
        {admin && file.status !== "downloading" && <div className="row-controls">
          {file.status !== "ready" && (file.sources?.includes(source) ?? true) && <EuiButton size="s" isDisabled={busy} aria-label={t("下载模型 {0}", file.label)}
            onClick={() => change("download")}>{file.status === "failed" ? t("重新下载") : t("下载模型")}</EuiButton>}
          {(file.status === "ready" || file.status === "failed" && file.downloaded_bytes > 0) && (deleting ? <>
            <span>{t("删除共享文件 {0}？", file.label)}</span>
            <EuiButton color="danger" size="s" isDisabled={busy || active} onClick={() => change("delete")}>{t("确认删除")}</EuiButton>
            <EuiButtonEmpty size="s" isDisabled={busy} onClick={() => setDeleting(false)}>{t("取消")}</EuiButtonEmpty>
          </> : <EuiButtonEmpty color="danger" size="s" isDisabled={busy || active}
            aria-label={t("删除模型 {0}", file.label)} onClick={() => { setDeleting(true); setError(""); }}>{t("删除模型文件")}</EuiButtonEmpty>)}
        </div>}
      {required.length > 0 && <details className="model-upload"><summary>{t("离线文件清单（{0}）", required.length)}</summary>
        <p>{t("安装时，服务器会按此模型的清单校验文件大小及 SHA-256 摘要。")}</p>
        <div className="model-filenames">{required.map(name => <code key={name}>{name}</code>)}</div>
        {admin && <ModelUpload id={file.id} label={file.label} required={required} available={file.status !== "ready" && file.status !== "downloading"}
          disabled={working || refreshing} working={setUploading} refresh={refresh} />}
      </details>}
      {error && <Failure error={error} />}{notice && (notice !== "已提交模型下载任务。" || file.status === "downloading" || file.status === "missing") && <p role="status">{t(notice)}</p>}
    </li>;
}
