import { useEffect, useState, type FormEvent } from "react";
import { EuiButton, EuiButtonGroup, EuiFieldText, EuiFormRow, EuiSelect, EuiTextArea } from "@elastic/eui";
import { api, message } from "./api";
import { read, t, write } from "./locale";
import { ChunkingSettings, type Chunking } from "./ChunkingSettings";
import { ModelSearch } from "./ModelSearch";
import { ModelUpload, customFiles } from "./ModelUpload";
import { ModelRow, statuses, type ModelFile } from "./ModelRow";
import { Failure } from "./shared";

type Backend = "builtin" | "ollama";
interface Catalog {
  backend: Backend;
  model: string;
  ollama_model: string;
  ollama_url: string;
  ollama_urls: string[];
  needs_rebuild: boolean;
  chunking: Chunking;
  models: ModelFile[];
}

function approvedUrl(raw: string) {
  try {
    const url = new URL(raw);
    return /^https?:$/.test(url.protocol) && !!url.hostname && !url.username && !url.password
      && !raw.includes("?") && !raw.includes("#") && raw.length <= 2048;
  } catch { return false; }
}

export function EmbeddingModels({ admin, jobActive, index, fallbackModel, active, openMarket }: {
  active: string; openMarket: () => void; admin: boolean; jobActive: boolean; index: (rebuild: boolean) => Promise<void>; fallbackModel: string;
}) {
  const [catalog, setCatalog] = useState<Catalog>();
  const [draft, setDraft] = useState<string>();
  const [draftUrl, setDraftUrl] = useState<string>();
  const [draftModel, setDraftModel] = useState<string>();
  const [serverList, setServerList] = useState<string>();
  const [marketView, setMarketView] = useState("discover");
  const [source, setSource] = useState(() => read("starstack.modelSource") === "huggingface" ? "huggingface" : "modelscope");
  const [writing, setWriting] = useState(false);
  const [loadedRevision, setLoadedRevision] = useState(0);
  const [revision, setRevision] = useState(0);
  const [actionError, setActionError] = useState("");
  const [readError, setReadError] = useState("");
  const [notice, setNotice] = useState("");
  const [chunkFeedback, setChunkFeedback] = useState(false);

  const busy = writing || loadedRevision !== revision;

  useEffect(() => {
    const controller = new AbortController();
    let timer: number;
    async function load() {
      let delay = 10000;
      try {
        const result = await api<Catalog>("models", undefined, controller.signal);
        if (controller.signal.aborted) return;
        if (!Array.isArray(result.models)) throw new Error(t("无法读取模型列表。"));
        setCatalog(result); setReadError("");
        if (result.models.some(item => item.status === "downloading")) delay = 1000;
      } catch (e) {
        if (!controller.signal.aborted) setReadError(message(e));
      }
      if (!controller.signal.aborted) { setLoadedRevision(revision); timer = window.setTimeout(load, delay); }
    }
    void load();
    return () => { controller.abort(); clearTimeout(timer); };
  }, [revision]);

  const files = catalog?.models ?? [];
  const current = catalog?.backend === "builtin" ? `builtin/${catalog.model}` : "ollama";
  const selection = draft ?? current;
  const selectedModel = files.find(row => selection === `builtin/${row.id}`);
  const urls = (catalog?.ollama_urls ?? []).filter(approvedUrl);
  const remoteUrl = draftUrl ?? catalog?.ollama_url ?? "";
  const remoteModel = (draftModel ?? catalog?.ollama_model ?? "").trim();
  const validModel = /^[a-zA-Z0-9._:/-]{1,256}$/.test(remoteModel);
  const available = selection === "ollama" ? urls.includes(remoteUrl) && validModel : selectedModel?.status === "ready";
  const changed = selection !== current || (selection === "ollama" && (remoteUrl !== catalog?.ollama_url || remoteModel !== catalog?.ollama_model));
  const servers = serverList ?? urls.join("\n");
  const editedUrls = [...new Set(servers.split("\n").map(line => line.trim()).filter(Boolean))];
  const validServers = editedUrls.length > 0 && editedUrls.length <= 32 && editedUrls.every(approvedUrl);
  const activeFile = catalog?.backend === "builtin" ? files.find(row => row.id === catalog.model) : undefined;
  const missingActiveFile = catalog?.backend === "builtin" && activeFile?.status !== "ready";

  async function change(action: "select" | "endpoints" | "chunking", body: object) {
    if (busy || (action !== "select" && action !== "chunking" && !admin)) return false;
    setWriting(true); setActionError(""); setNotice(""); setChunkFeedback(action === "chunking");
    try {
      await api<{ message: string }>(`models/${action}`, body);
      if (action === "select") { setDraft(undefined); setDraftUrl(undefined); setDraftModel(undefined); }
      if (action === "endpoints") setServerList(undefined);
      setNotice({ select: "已更新当前账号的模型。", chunking: "已保存分块设置。", endpoints: "已保存 Ollama 服务地址。" }[action]);
      setRevision(n => n + 1);
      return true;
    } catch (e) { setActionError(message(e)); return false; } finally { setWriting(false); }
  }

  function apply(e: FormEvent) {
    e.preventDefault();
    if (changed && available) void change("select", selectedModel
      ? { backend: "builtin", model: selectedModel.id } : { backend: "ollama", url: remoteUrl, model: remoteModel });
  }

  async function updateIndex(rebuild: boolean) {
    await index(rebuild);
    setRevision(n => n + 1);
  }

  const feedback = <>{actionError && <Failure error={actionError} />}{notice && <p role="status">{t(notice)}</p>}</>;

  function changeSource(value: string) {
    setSource(value);
    write("starstack.modelSource", value);
  }

  return <>
    <div className="operations" hidden={active !== "index"} role="tabpanel" id="maintenance-panel-index" aria-labelledby="maintenance-tab-index">
    <section className="operation"><div><h2>{t("语义索引")}</h2>
    <p>{t("为新增或变更的仓库建立索引。更换模型后可重建全部索引。")}</p></div>
    <dl className="inline-fact"><dt>{t("当前账号模型")}</dt><dd>{catalog
      ? `${catalog.backend === "ollama" ? "Ollama" : t("内置")} · ${catalog.model}` : fallbackModel || t("未配置")}</dd></dl>
    {readError && <Failure error={readError} retry={() => setRevision(n => n + 1)} />}
    {!catalog && !readError && <p role="status">{t("正在读取模型状态…")}</p>}
    {catalog && <>
      <form className="model-selector" onSubmit={apply}>
        <EuiFormRow label={t("切换检索模型")} fullWidth><EuiSelect fullWidth value={selection} disabled={busy || jobActive}
          onChange={e => { setDraft(e.target.value); setNotice(""); setActionError(""); }}
          options={[
            { value: "ollama", text: `Ollama · ${catalog.ollama_model}` },
            ...files.map(file => ({ value: `builtin/${file.id}`, disabled: file.status !== "ready",
              text: `${t("内置")} · ${file.label}${file.status === "ready" ? "" : ` (${t(statuses[file.status])})`}` })),
          ]} /></EuiFormRow>
        {selection === "ollama" && <>
          <EuiFormRow label={t("Ollama 服务地址")} fullWidth><EuiSelect fullWidth value={urls.includes(remoteUrl) ? remoteUrl : ""}
            disabled={busy || jobActive} onChange={e => setDraftUrl(e.target.value)}
            options={[...(!urls.includes(remoteUrl) ? [{ value: "", text: t("选择服务地址"), disabled: true }] : []), ...urls.map(url => ({ value: url, text: url }))]} /></EuiFormRow>
          <EuiFormRow label={t("Ollama 模型名")} isInvalid={!validModel} error={t("请输入有效的 Ollama 模型名（可含标签）")} fullWidth><EuiFieldText required fullWidth isInvalid={!validModel} value={draftModel ?? catalog.ollama_model}
            maxLength={256} disabled={busy || jobActive} onChange={e => setDraftModel(e.target.value)} /></EuiFormRow>
          <p>{t("建立索引时，所选 Ollama 服务会收到仓库文本和你的备注；搜索时会收到查询文本。")}</p>
        </>}
        <EuiButton type="submit" isDisabled={busy || jobActive || !available || !changed}>{t("使用此模型")}</EuiButton>
      </form>
      {missingActiveFile && <p>{admin ? t("当前模型就绪后即可建立索引。") : t("当前内置模型尚未就绪。请联系管理员下载，或切换到可用模型。")}</p>}
      {catalog.chunking && <ChunkingSettings key={catalog.backend} current={catalog.chunking} backend={catalog.backend === "ollama" ? "Ollama" : t("内置")}
        disabled={busy || jobActive} save={settings => change("chunking", settings)} />}
      {chunkFeedback && feedback}
      {catalog.needs_rebuild && <p className="index-hint">{t("当前账号的索引需要重建。")}</p>}
    </>}
    <div className="row-controls">
      <EuiButton isDisabled={jobActive || busy || missingActiveFile || catalog?.needs_rebuild} onClick={() => updateIndex(false)}>{t("更新索引")}</EuiButton>
      <EuiButton isDisabled={jobActive || busy || missingActiveFile} onClick={() => updateIndex(true)}>{t("重建索引")}</EuiButton>
    </div>
    <EuiButton onClick={openMarket}>{t("打开模型市场")}</EuiButton>
    {admin && catalog && <details className="model-endpoints"><summary>{t("管理 Ollama 服务地址")}</summary>
      <form className="model-selector" onSubmit={e => { e.preventDefault(); if (validServers) void change("endpoints", { urls: editedUrls }); }}>
        <EuiFormRow label={t("此实例允许使用的服务地址")} helpText={t("每行一个 HTTP(S) 地址。可填写 1 至 32 个地址，请勿包含凭证、查询参数或片段。")} fullWidth>
          <EuiTextArea fullWidth rows={3} spellCheck={false} value={servers} disabled={busy} onChange={e => setServerList(e.target.value)} /></EuiFormRow>
        <EuiButton type="submit" isDisabled={busy || !validServers || editedUrls.join("\n") === urls.join("\n")}>{t("保存服务地址")}</EuiButton>
      </form>
    </details>}
    {!chunkFeedback && feedback}
    </section></div>
    <div className="operations market-panel" hidden={active !== "market"} role="tabpanel" id="maintenance-panel-market" aria-labelledby="maintenance-tab-market">
      <section className="operation">
        <div className="market-heading"><div><h2>{t("模型市场")}</h2><p>{t("查找、安装和管理本地语义检索模型。模型文件由本实例的账号共用。")}</p></div>
          <EuiButtonGroup legend={t("模型市场视图")} idSelected={marketView}
            options={[{ id: "discover", label: t("发现模型") }, { id: "installed", label: t("已安装") }]}
            onChange={setMarketView} />
        </div>
        {readError && <Failure error={readError} retry={() => setRevision(n => n + 1)} />}
        {!catalog && !readError && <p role="status">{t("正在读取模型状态…")}</p>}
        {catalog && <>
          <div className="market-view" hidden={marketView !== "discover"}>
            <ModelSearch models={files} admin={admin} source={source} changeSource={changeSource} refresh={() => setRevision(n => n + 1)} />
            <h3>{t("推荐模型")}</h3>
            <ul className="model-grid">{files.filter(file => !!file.sources?.length).map(file =>
              <ModelRow key={file.id} file={file} admin={admin} active={activeFile?.id === file.id}
                source={source} refreshing={busy} refresh={() => setRevision(n => n + 1)} />)}</ul>
          </div>
          <div className="market-view" hidden={marketView !== "installed"}>
            <h3>{t("已安装与下载任务")}</h3>
            {files.some(file => file.status !== "missing") ? <ul className="model-grid">{files.filter(file => file.status !== "missing").map(file =>
              <ModelRow key={file.id} file={file} admin={admin} active={activeFile?.id === file.id}
                source={source} refreshing={busy} refresh={() => setRevision(n => n + 1)} />)}</ul> :
              <div className="model-empty"><p>{t("还没有本地模型。")}</p><EuiButton onClick={() => setMarketView("discover")}>{t("发现模型")}</EuiButton></div>}
          </div>
          {admin && <details className="model-endpoints model-custom"><summary>{t("从本机导入模型")}</summary>
            <p>{t("选择模型根目录中的三个文件，再单独选择 pooling 配置。服务器会检查模型架构。")}</p>
            <div className="model-filenames">{customFiles.map(name => <code key={name}>{name}</code>)}</div>
            <ModelUpload id="custom" label={t("自定义兼容模型")} required={customFiles} disabled={busy} refresh={() => setRevision(n => n + 1)} />
          </details>}
        </>}
      </section>
    </div>
  </>;
}
