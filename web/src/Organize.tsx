import { useEffect, useRef, useState } from "react";
import { EuiButton, EuiButtonEmpty, EuiCheckbox, EuiFlyout, EuiFlyoutBody, EuiFlyoutFooter, EuiFlyoutHeader, EuiFormRow, EuiSelect } from "@elastic/eui";
import { api, message, type Filters } from "./api";
import { t } from "./locale";
import { OrganizeJob, type Target } from "./OrganizeJob";
import { Failure, Icon } from "./shared";

interface Suggestion { id: number; full_name: string; html_url: string; tags: string[]; reason: string }
interface Batch { suggestions: Suggestion[]; scanned: number; next_offset: number | null; total: number }

export function Organize({ filter, scope, close, refresh }: {
  filter: Filters; scope: string[]; close: () => void; refresh: () => void;
}) {
  const [target, setTarget] = useState<Target>("tags");
  const github = target === "github_lists";
  const [batch, setBatch] = useState<Batch>();
  const [selected, setSelected] = useState<number[]>([]);
  const [applied, setApplied] = useState<number[]>([]);
  const [busy, setBusy] = useState<"preview" | "apply">();
  const [failure, setFailure] = useState("");
  const [previewFailed, setPreviewFailed] = useState(false);
  const [result, setResult] = useState<{ updated: number; added: number }>();
  const request = useRef<AbortController | null>(null);
  useEffect(() => () => request.current?.abort(), []);

  async function preview(offset: number) {
    if (request.current) return;
    const controller = new AbortController();
    request.current = controller; setBusy("preview"); setFailure(""); setPreviewFailed(false);
    try {
      const data = await api<Batch>("organize/preview", { filter, limit: 20, offset, target }, controller.signal);
      if (controller.signal.aborted) return;
      setBatch(data); setSelected([]); setApplied([]); setResult(undefined);
    } catch (e) {
      if (!controller.signal.aborted) { setFailure(message(e)); setPreviewFailed(true); }
    } finally { request.current = null; if (!controller.signal.aborted) setBusy(undefined); }
  }

  const remaining = batch?.suggestions.filter(repo => !applied.includes(repo.id)) ?? [];
  async function apply() {
    if (request.current || !selected.length) return;
    const items = remaining.filter(s => selected.includes(s.id)).map(({ id, tags }) => ({ id, tags }));
    if (!items.length) return;
    const controller = new AbortController();
    request.current = controller; setBusy("apply"); setFailure(""); setPreviewFailed(false);
    try {
      const changes = await api<{ updated: number; added: number }>("organize/apply", { items, target }, controller.signal);
      if (controller.signal.aborted) return;
      setResult(changes); setApplied(ids => [...ids, ...items.map(row => row.id)]); setSelected([]); refresh();
    } catch (e) { if (!controller.signal.aborted) setFailure(message(e)); }
    finally { request.current = null; if (!controller.signal.aborted) setBusy(undefined); }
  }
  const dismiss = () => { if (busy !== "apply") close(); };
  return <EuiFlyout size="s" ownFocus onClose={dismiss} aria-labelledby="organize-heading">
    <EuiFlyoutHeader hasBorder><h2 id="organize-heading">{t("AI 整理")}</h2></EuiFlyoutHeader>
    <EuiFlyoutBody><div className="organize-body">
      <p>{t("整理时，AI 服务会收到仓库的名称、简介、语言、GitHub 主题、已有标签及 GitHub 收藏分类名，其中可能包含私有仓库和私有分类的信息。")}</p>
      <div><h3>{t("本次范围")}</h3>{scope.length ? <div className="chips">{scope.map((item,i) => <span key={i}>{item}</span>)}</div>
        : <p>{t("全部收藏")}</p>}</div>
      <EuiFormRow label={t("整理到")} fullWidth><EuiSelect value={target} fullWidth disabled={!!busy}
        options={[{ value: "tags", text: t("本地标签") }, { value: "github_lists", text: "GitHub Lists" }]}
        onChange={e => { setTarget(e.target.value as Target); setBatch(undefined); setResult(undefined); setFailure(""); setSelected([]); setApplied([]); }} /></EuiFormRow>
      {github && <p>{t("确认添加后，StarStack 会将仓库加入你已有的 GitHub 收藏分类，需要 GitHub 写入权限。只添加成员，保留原有分类及成员。")}</p>}
      <section className="organize-manual"><h3>{t("逐批预览")}</h3><p>{t("每次预览最多 20 个仓库。勾选建议后确认添加。")}</p>
      {!batch && <EuiButton fill isLoading={busy === "preview"} onClick={() => preview(0)}>{github ? t("预览分类建议") : t("预览标签建议")}</EuiButton>}
      {failure && <div><Failure error={failure} />{previewFailed && <p>{t("请重试预览；如果仍然失败，请联系管理员检查 AI 服务及对话模型配置。")}</p>}</div>}
      {batch && <>
        <p role="status">{t("本批查看了 {0} 个仓库，筛选范围共 {1} 个。", batch.scanned, batch.total)}</p>
        {!!remaining.length && <div>
          <EuiCheckbox id="organize-all" checked={selected.length === remaining.length} indeterminate={selected.length > 0 && selected.length < remaining.length}
            disabled={!!busy} label={t("选择本批全部建议")} onChange={e => setSelected(e.target.checked ? remaining.map(s => s.id) : [])} />
          <ul className="organize-suggestions">{remaining.map(row => <li key={row.id}>
            <EuiCheckbox id={`organize-${row.id}`} checked={selected.includes(row.id)} label={row.full_name} disabled={!!busy}
              onChange={e => setSelected(previous => e.target.checked ? [...previous,row.id] : previous.filter(id => id !== row.id))} />
            <div className="suggestion-detail"><div className="chips">{row.tags.map(tag => <span key={tag}>{tag}</span>)}</div>
              {row.reason && <p>{row.reason}</p>}{/^https?:\/\//i.test(row.html_url) && <a href={row.html_url} target="_blank" rel="noreferrer"
                aria-label={t("在 GitHub 查看 {0}", row.full_name)}>GitHub <Icon type="popout" /></a>}</div>
          </li>)}</ul>
        </div>}
        {result && <p role="status">{t(github ? "已更新 {0} 个仓库，添加 {1} 项分类成员关系。" : "已更新 {0} 个仓库，添加 {1} 个标签。", result.updated, result.added)}</p>}
        {!batch.scanned ? <p>{t("当前范围没有可整理的仓库。")}</p> : !batch.suggestions.length && <p>{t(github ? "本批没有新的分类建议。" : "本批没有新的标签建议。")}</p>}
      </>}
      </section>
      <OrganizeJob filter={filter} target={target} filtered={!!scope.length} refresh={refresh} />
    </div></EuiFlyoutBody>
    <EuiFlyoutFooter><div className="drawer-actions">
      <EuiButtonEmpty onClick={dismiss} isDisabled={busy === "apply"}>{t("关闭")}</EuiButtonEmpty>
      {batch?.next_offset != null && <EuiButton isDisabled={!!busy} isLoading={busy === "preview"} onClick={() => preview(batch.next_offset!)}>{t("预览下一批")}</EuiButton>}
      {!!remaining.length && <EuiButton fill isDisabled={!!busy || selected.length === 0} isLoading={busy === "apply"} onClick={apply}>
        {t(github ? "确认添加到 GitHub（{0}）" : "确认添加标签（{0}）", selected.length)}</EuiButton>}
    </div></EuiFlyoutFooter>
  </EuiFlyout>;
}
