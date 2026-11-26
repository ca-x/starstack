import { useEffect, useRef, useState } from "react";
import { EuiButton } from "@elastic/eui";
import { t } from "./locale";
import { Failure } from "./shared";

export const customFiles = ["config.json", "tokenizer.json", "model.safetensors", "1_Pooling/config.json"];
const poolingPath = "1_Pooling/config.json";

export function ModelUpload({ id, label, required, available = true, disabled, working, refresh }: {
  id: string; label: string; required: string[]; available?: boolean; disabled: boolean; working?: (busy: boolean) => void; refresh: () => void;
}) {
  const [picked, setPicked] = useState<File[]>([]);
  const [pooling, setPooling] = useState<File>();
  const [uploaded, setUploaded] = useState<{ loaded: number; total: number }>();
  const [error, setError] = useState("");
  const [saved, setSaved] = useState(false);
  const filesInput = useRef<HTMLInputElement>(null);
  const poolingInput = useRef<HTMLInputElement>(null);
  const xhr = useRef<XMLHttpRequest>();
  useEffect(() => () => xhr.current?.abort(), []);
  const topLevel = required.filter(name => name !== poolingPath);
  const needsPooling = required.includes(poolingPath);
  function manifestName(file: File) {
    return topLevel.includes("model.gguf") && file.name.toLowerCase().endsWith(".gguf") ? "model.gguf" : file.name;
  }
  const valid = picked.length === topLevel.length && topLevel.every(name => picked.some(file => manifestName(file) === name && file.size > 0))
    && (!needsPooling || (pooling?.name === "config.json" && pooling.size > 0));

  function upload() {
    if (disabled || !available || !valid || xhr.current) return;
    const data = new FormData();
    for (const file of picked) data.append("files", file, manifestName(file));
    if (needsPooling && pooling) data.append("pooling_config", pooling);
    const request = new XMLHttpRequest();
    xhr.current = request;
    request.open("POST", "/api/models/upload?model=" + encodeURIComponent(id));
    request.withCredentials = true;
    request.setRequestHeader("X-StarStack", "1");
    request.responseType = "json";
    request.upload.onprogress = event => setUploaded({ loaded: event.loaded, total: event.lengthComputable ? event.total : 0 });
    request.onload = () => {
      if (request.status === 401) { location.replace("/login"); return; }
      if (request.status < 200 || request.status >= 300) {
        setError(request.response?.error ? t(request.response.error) : t("请求失败（HTTP {0}）", request.status));
        return;
      }
      setSaved(true); setPicked([]); setPooling(undefined);
      if (filesInput.current) filesInput.current.value = "";
      if (poolingInput.current) poolingInput.current.value = "";
      refresh();
    };
    request.onerror = () => setError(t("上传连接中断，请重新上传。"));
    request.onloadend = () => { xhr.current = undefined; setUploaded(undefined); working?.(false); };
    setUploaded({ loaded: 0, total: 0 }); working?.(true); setSaved(false); setError("");
    request.send(data);
  }
  return <>
    {(available || uploaded) && <div className="model-upload-controls">
      <label>{t(needsPooling ? "选择 {0} 的根目录文件" : "选择 {0} 的全部文件", label)}
        <input ref={filesInput} type="file" multiple disabled={disabled || !!uploaded} onChange={e => {
          setPicked(Array.from(e.target.files ?? [])); setError(""); setSaved(false);
        }} /></label>
      {topLevel.includes("model.gguf") && <p>{t("GGUF 权重可保留原文件名，服务器会按摘要校验。")}</p>}
      {needsPooling && <label>{t("选择 1_Pooling/config.json")}
        <input ref={poolingInput} type="file" disabled={disabled || !!uploaded} onChange={e => {
          setPooling(e.target.files?.[0]); setError(""); setSaved(false);
        }} /></label>}
      {(picked.length > 0 || pooling) && !valid && <p role="alert">{t("请按清单选齐文件。文件名须一致，文件不能为空。")}</p>}
      <EuiButton size="s" isDisabled={disabled || !!uploaded || !valid} onClick={upload}>{t("上传 {0}", label)}</EuiButton>
    </div>}
    {uploaded && <div className="model-progress" role="status">
      <progress aria-label={t("{0} 上传进度", label)} max={uploaded.total || undefined} value={uploaded.total ? uploaded.loaded : undefined} />
      <small>{uploaded.total > 0 && uploaded.loaded === uploaded.total ? t("上传完毕，正在等待服务器校验…") : t("正在上传 {0}，请保持页面打开。", label)}</small>
    </div>}
    {error && <Failure error={error} />}{saved && <p role="status">{t("模型文件已上传。")}</p>}
  </>;
}
