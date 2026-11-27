import { useEffect, useRef, useState, type FormEvent } from "react";
import { EuiButton, EuiCheckbox, EuiFieldText, EuiFormRow, EuiSelect } from "@elastic/eui";
import { api, message } from "./api";
import { t } from "./locale";
import type { ModelFile } from "./ModelRow";
import { Failure } from "./shared";

interface Hit { repository: string; name: string; downloads: number | null; source: string; url: string; format: string | null; model_id: string | null }

function SearchResult({ hit, model, admin, refresh }: { hit: Hit; model?: ModelFile; admin: boolean; refresh: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [sent, setSent] = useState(false);
  async function install() {
    if (!admin || busy) return;
    setBusy(true); setError(""); setSent(false);
    try {
      if (hit.model_id) await api("models/download", { id: hit.model_id, source: hit.source });
      else await api("models/hub-install", { source: hit.source, repository: hit.repository });
      setSent(true); refresh();
    } catch (e) { setError(message(e)); } finally { setBusy(false); }
  }
  return <li><div className="model-file-header"><div><strong>{/^https?:\/\//i.test(hit.url) ? <a href={hit.url} target="_blank" rel="noreferrer">{hit.repository}</a> : hit.repository}</strong>
    {hit.name && hit.name !== hit.repository && <small>{hit.name}</small>}</div></div>
    <div className="metadata">{hit.format && <span>{hit.format}</span>}{hit.downloads != null && <span>{t("{0} 次下载", hit.downloads.toLocaleString())}</span>}
      {hit.model_id && <span>{t("已适配")}</span>}{model?.status === "ready" && <span>{t("已安装")}</span>}</div>
    {admin && model?.status !== "ready" && <div className="row-controls"><EuiButton size="s" isDisabled={busy || model?.status === "downloading"} isLoading={busy}
      onClick={install}>{hit.model_id ? t("下载并安装") : t("检查兼容性并安装")}</EuiButton></div>}
    {error && <Failure error={error} />}{sent && <p role="status">{t("安装请求已完成，请查看本机模型列表。")}</p>}
  </li>;
}

export function ModelSearch({ admin, models, source, changeSource, refresh }: {
  admin: boolean; models: ModelFile[]; source: string; changeSource: (source: string) => void; refresh: () => void;
}) {
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<Hit[]>();
  const [format, setFormat] = useState("");
  const [verified, setVerified] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const request = useRef<AbortController>();
  useEffect(() => () => request.current?.abort(), []);

  async function search(event: FormEvent) {
    event.preventDefault();
    if (!query.trim() || busy) return;
    const controller = new AbortController();
    request.current = controller; setBusy(true); setError(""); setResults(undefined); setFormat("");
    try {
      const data = await api<{ results: Hit[] }>("models/search", { source, query: query.trim() }, controller.signal);
      if (!controller.signal.aborted) setResults(data.results);
    } catch (e) { if (!controller.signal.aborted) setError(message(e)); }
    finally { if (!controller.signal.aborted) setBusy(false); }
  }
  const formats = [...new Set(results?.map(h => h.format).filter((item): item is string => !!item))].sort();
  const visible = results?.filter(item => (!format || item.format === format) && (!verified || !!item.model_id));
  return <div className="market-search"><form className="market-search-form" onSubmit={search}>
      <EuiFormRow label={t("仓库名称或关键词")} fullWidth>
        <EuiFieldText fullWidth type="search" placeholder={t("例如 MiniLM 或 Qwen3 Embedding")} value={query} disabled={busy}
          onChange={e => setQuery(e.target.value)} />
      </EuiFormRow>
      <EuiFormRow label={t("模型仓库")} fullWidth><EuiSelect fullWidth disabled={busy} value={source}
        options={[{ value: "modelscope", text: t("魔搭 ModelScope") }, { value: "huggingface", text: "Hugging Face" }]}
        onChange={e => { changeSource(e.target.value); setResults(undefined); setError(""); }} /></EuiFormRow>
      <EuiButton type="submit" isLoading={busy} isDisabled={busy || !query.trim()}>{t("搜索仓库")}</EuiButton>
    </form>
    {error && <Failure error={error} />}
    {results && <div className="model-search-results">
      {results.length > 0 && <><p>{t("只有已适配的嵌入模型架构可以安装。")}</p><div className="row-controls">
        <EuiSelect compressed aria-label={t("模型格式")} value={format} options={[{ value: "", text: t("全部格式") }, ...formats.map(f => ({ value: f, text: f }))]} onChange={e => setFormat(e.target.value)} />
        <EuiCheckbox id="model-search-verified" label={t("仅可直接安装")} checked={verified} onChange={e => setVerified(e.target.checked)} />
      </div></>}
      {visible?.length ? <ul className="model-grid">{visible.map(hit => <SearchResult key={`${hit.source}/${hit.repository}`} hit={hit} model={models.find(m => m.id === hit.model_id)}
        admin={admin} refresh={refresh} />)}</ul> : <p role="status">{results.length ? t("没有符合筛选条件的模型。") : t("没有找到模型仓库。")}</p>}
    </div>}
  </div>;
}
