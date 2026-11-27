import { languageName, t } from "./locale";
import { useEffect, useRef, useState, type FormEvent } from "react";
import { EuiButton, EuiButtonEmpty, EuiFieldText, EuiSelect } from "@elastic/eui";
import { api, date, message, request, type Filters, type Repo, type Sort, type Stats } from "./api";
import { TagCloud } from "./TagCloud";
import { Organize } from "./Organize";
import { Detail } from "./Detail";
import { FilterPanel } from "./Filters";
import { Failure, Icon, Language } from "./shared";

function RepoRow({ repo, open, source }: { repo: Repo; open: () => void; source?: number }) {
  const [owner, name] = repo.full_name.split("/");
  return <li className="repo-row">
    <span className="repo-symbol" aria-hidden="true"><Icon type={repo.pinned ? "pin" : "document"} /></span>
    <div className="repo-copy">
      <button className="repo-title" onClick={open} aria-label={t("查看 {0}", repo.full_name)}>
        {name ? <><span>{owner} / </span><strong>{name}</strong></> : <strong>{owner}</strong>}
      </button>
      {repo.description && <p>{repo.description}</p>}
      {repo.note.trim() && <div className="repo-note">
        <Icon type="pencil" /><span className="note-label">{t("我的备注")}</span><span className="note-preview">{repo.note}</span>
      </div>}
      <div className="metadata">
        {typeof repo.score === "number" && Number.isFinite(repo.score) &&
          <span className="match-score" title={t("余弦相似度")}>{t("相关度 {0}", repo.score.toFixed(3))}</span>}
        {repo.language && <Language name={repo.language} />}
        <span aria-label={t("{0} 个 Star", repo.stargazers_count)}><Icon type="starEmpty" />{repo.stargazers_count.toLocaleString()}</span>
        <span>{t("收藏于")} {date(repo.starred_at)}</span>
        {source && /^https?:\/\//i.test(repo.html_url) && <a href={repo.html_url} target="_blank" rel="noreferrer">[{source}] {t("GitHub 来源")}<Icon type="popout" /></a>}
        {repo.archived && <span className="tag">{t("已归档")}</span>}
        {repo.pinned && <span className="tag">{t("已置顶")}</span>}
        {repo.tags.map(tag => <span className="tag" key={tag}>{tag}</span>)}
      </div>
    </div>
  </li>;
}

export function Library({ stats, initial, revision, onEdit, onSync, tagLink }: {
  stats: Stats; initial: Filters; revision: number; onEdit: () => void; onSync: () => void; tagLink: (tag: string) => string;
}) {
  const [filters, setFilters] = useState(initial);
  const [sort, setSort] = useState<Sort>("time");
  const [input, setInput] = useState("");
  const [query, setQuery] = useState("");
  const [mode, setMode] = useState<"search" | "ask">("search");
  const [answer, setAnswer] = useState("");
  const [page, setPage] = useState(0);
  const [pageSize, setPageSize] = useState(25);
  const [showFilters, setShowFilters] = useState(false);
  const [repos, setRepos] = useState<Repo[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [exportError, setExportError] = useState("");
  const [exporting, setExporting] = useState(false);
  const [retry, setRetry] = useState(0);
  const [selected, setSelected] = useState<Repo>();
  const [organizing, setOrganizing] = useState(false);
  const inputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    setError("");
    setAnswer("");
    const body = query ? { ...filters, query, ...(mode === "search" && { limit: pageSize }) } : { ...filters, sort, limit: pageSize, offset: page * pageSize };
    api<{ repositories: Repo[]; total?: number; answer?: string }>(query ? mode : "list", body, controller.signal)
      .then(data => {
        if (controller.signal.aborted) return;
        const count = data.total ?? data.repositories.length;
        if (!query && page > 0 && page * pageSize >= count) {
          setPage(Math.max(0, Math.ceil(count / pageSize) - 1));
          return;
        }
        setAnswer(data.answer || "");
        setRepos(data.repositories);
        setTotal(count);
      })
      .catch(e => { if (!controller.signal.aborted) setError(message(e)); })
      .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [filters, sort, page, pageSize, query, mode, revision, retry]);

  function search(e: FormEvent) {
    e.preventDefault();
    setPage(0);
    setQuery(input.trim());
    if (input.trim() === query) setRetry(i => i + 1);
  }

  async function download(format: "markdown" | "json") {
    if (exporting) return;
    setExporting(true);
    setExportError("");
    try {
      const response = await request("export", { ...filters, sort, format });
      const href = URL.createObjectURL(await response.blob());
      const link = Object.assign(document.createElement("a"), { href, download: "starstack." + (format === "markdown" ? "md" : "json") });
      document.body.append(link);
      link.click();
      link.remove();
      window.setTimeout(() => URL.revokeObjectURL(href), 1000);
    } catch (e) { setExportError(message(e)); } finally { setExporting(false); }
  }

  const activeFilters = [filters.language && languageName(filters.language), filters.tag, filters.list && t("GitHub 分类 · {0}", filters.list), filters.topic, filters.since && t("从 {0}", filters.since), filters.until && t("至 {0}", filters.until),
    filters.archived !== undefined && (filters.archived ? t("已归档") : t("活跃"))].filter(Boolean);
  const emptyLibrary = stats.total === 0 && !query && !activeFilters.length && !filters.pinned;
  return <>
    <div className="page-heading"><div><h1>{initial.pinned ? t("置顶仓库") : initial.tag ? t("标签 · {0}", initial.tag) : t("全部收藏")}</h1>
      <p>{initial.pinned ? t("优先查看常用的仓库。") : initial.tag ? t("查看使用这个标签的收藏。") : t("浏览、整理并检索你的 GitHub Stars。")}</p></div>
    </div>
    <section className="search-panel" aria-label={t("搜索收藏")}>
      <div className="search-modes" role="group" aria-label={t("检索方式")}>
        {(["search","ask"] as const).map(value => <button key={value} type="button" aria-pressed={mode === value}
          onClick={() => { if (mode !== value) { setMode(value); setQuery(""); setPage(0); } }}>
          <Icon type={value === "search" ? "search" : "sparkles"} />{value === "search" ? t("语义搜索") : t("AI 推荐")}</button>)}
      </div>
      <form className="search-form" onSubmit={search}>
        <EuiFieldText inputRef={el => { inputRef.current = el; }} fullWidth icon="search" aria-label={mode === "ask" ? t("向 AI 提问") : t("自然语言搜索")} type="search"
          placeholder={mode === "ask" ? t("描述你的需求，例如「推荐适合自建的笔记工具，并说明理由」") : t("用自然语言搜索，例如「适合离线使用的笔记工具」")} value={input}
          onChange={e => { setInput(e.target.value); if (!e.target.value) { setQuery(""); setPage(0); } }} />
        <EuiButton fill type="submit" isDisabled={mode === "ask" && !input.trim()}>{mode === "ask" ? t("提问") : t("搜索")}</EuiButton>
      </form>
      <div className="filter-bar">
        <button className="quiet-control" aria-expanded={showFilters} aria-controls="filter-panel" onClick={() => setShowFilters(!showFilters)}>
          <Icon type="filter" />{t("筛选条件")}{activeFilters.length > 0 && <b>{activeFilters.length}</b>}
        </button>
        {!query && <button className="quiet-control" aria-haspopup="dialog" onClick={() => setOrganizing(true)}>
          <Icon type="sparkles" />{t("AI 整理")}</button>}
        {activeFilters.map((name, i) => <span className="tag" key={i}>{name}</span>)}
        {query && <button className="quiet-control" onClick={() => { setInput(""); setQuery(""); setPage(0); inputRef.current?.focus(); }}><Icon type="cross" />{t("结束搜索")}</button>}
      </div>
      {showFilters && <div id="filter-panel"><FilterPanel initial={filters} stats={stats} fixedTag={initial.tag} apply={next => { setFilters(next); setPage(0); setShowFilters(false); }} /></div>}
    </section>
    {!initial.pinned && !initial.tag && !query && Object.keys(stats.tags).length > 0 &&
      <TagCloud values={stats.tags} tagLink={tagLink} compact />}
    {answer && <section className="ai-answer"><h2>{t("推荐结果")}</h2><p>{answer}</p></section>}
    <section className="collection" aria-labelledby="results-heading">
      <div className="result-head"><h2 id="results-heading">{query ? (mode === "ask" ? t("引用仓库") : t("相关仓库")) : t("仓库列表")}
        {!loading && !error && <span className="count">{total.toLocaleString()}</span>}</h2>
        <div className="row-controls">
          {!query && <EuiSelect compressed aria-label={t("排序")} value={sort} onChange={e => { setSort(e.target.value as Sort); setPage(0); }}
            options={[{ value: "time", text: t("按收藏时间") }, { value: "stars", text: t("按 Star 数") }]} />}
          {!(query && mode === "ask") && <EuiSelect compressed aria-label={query ? t("最多返回条数") : t("每页条数")} value={pageSize} onChange={e => { setPageSize(Number(e.target.value)); setPage(0); }}
            options={[10, 25, 50, 100].map(n => ({ value: n, text: query ? t("前 {0} 条", n) : t("{0} 条 / 页", n) }))} />}
        </div>
      </div>
      {error ? <Failure error={error} retry={() => setRetry(i => i + 1)} /> : loading ?
        <div className="empty" role="status">{query ? t("正在检索收藏…") : t("正在读取仓库…")}</div> : repos.length ?
        <ul className="repo-list">{repos.map((repo, index) => <RepoRow key={repo.id} repo={repo} source={query && mode === "ask" ? index + 1 : undefined} open={() => setSelected(repo)} />)}</ul> :
        <div className="empty"><span className="empty-star" aria-hidden="true">✦</span><h3>{emptyLibrary ? t("还没有收藏") : t("没有找到仓库")}</h3>
          <p>{emptyLibrary ? t("从 GitHub 同步 Stars，建立你的仓库收藏。") : t("可以调整筛选条件或换一种描述再试。")}</p>
          {emptyLibrary && <EuiButton size="s" onClick={onSync}>{t("前往同步")}</EuiButton>}
        </div>}
      {!query && !error && !loading && total > pageSize && <nav className="pagination" aria-label={t("分页")}>
        <EuiButtonEmpty onClick={() => setPage(page - 1)} isDisabled={page === 0} iconType="arrowLeft">{t("上一页")}</EuiButtonEmpty>
        <span>{t("第 {0} / {1} 页", page + 1, Math.ceil(total / pageSize))}</span>
        <EuiButtonEmpty onClick={() => setPage(page + 1)} isDisabled={(page + 1) * pageSize >= total} iconType="arrowRight" iconSide="right">{t("下一页")}</EuiButtonEmpty>
      </nav>}
    </section>
    {!query && <div className="export-bar"><span><Icon type="download" />{t("导出筛选范围内的仓库")}</span>
      <EuiButtonEmpty size="s" isDisabled={exporting} onClick={() => download("markdown")}>Markdown</EuiButtonEmpty>
      <EuiButtonEmpty size="s" isDisabled={exporting} onClick={() => download("json")}>JSON</EuiButtonEmpty>
    </div>}
    {exportError && <Failure error={exportError} />}
    {organizing && <Organize filter={filters} scope={[...(filters.pinned ? [t("已置顶")] : []), ...activeFilters.filter((name): name is string => typeof name === "string")]}
      close={() => setOrganizing(false)} refresh={onEdit} />}
    {selected && <Detail key={selected.id} repo={selected} close={() => setSelected(undefined)} saved={() => { setSelected(undefined); onEdit(); }} />}
  </>;
}
