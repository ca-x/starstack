import { t } from "./locale";
import { useState, type FormEvent } from "react";
import {
  EuiButton, EuiCheckbox, EuiFieldText, EuiFlyout, EuiFlyoutBody, EuiFlyoutFooter,
  EuiFlyoutHeader, EuiFormRow, EuiTextArea,
} from "@elastic/eui";
import { api, date, message, type Repo } from "./api";
import { RepositoryCommits } from "./RepositoryCommits";
import { RepositoryConversation, draftSummary } from "./RepositoryAi";
import { Failure, Icon, Language } from "./shared";

export function Detail({ repo, close, saved }: { repo: Repo; close: () => void; saved: () => void }) {
  const [tags, setTags] = useState(repo.tags.join(", "));
  const [note, setNote] = useState(repo.note);
  const [pinned, setPinned] = useState(repo.pinned);
  const [summary, setSummary] = useState(repo.summary ?? "");
  const [generating, setGenerating] = useState(false);
  const [summaryError, setSummaryError] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const normalizedTags = [...new Set(tags.split(/[,，\n]/).map(tag => tag.trim()).filter(Boolean))];
  const annotationsChanged = note !== repo.note || pinned !== repo.pinned || JSON.stringify(normalizedTags) !== JSON.stringify(repo.tags);
  const summaryChanged = summary !== (repo.summary ?? "");
  const changed = annotationsChanged || summaryChanged;
  async function generate() {
    if (generating || busy || (summary.trim() && !window.confirm(t("用新草稿替换当前总结？")))) return;
    setGenerating(true); setSummaryError("");
    try { setSummary(await draftSummary(repo.id)); }
    catch (e) { setSummaryError(message(e)); } finally { setGenerating(false); }
  }
  function dismiss() {
    if (!busy && !generating && (!changed || window.confirm(t("放弃尚未保存的更改？")))) close();
  }
  const url = /^https?:\/\//i.test(repo.html_url) ? repo.html_url : undefined;

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy || generating || !changed) return;
    setBusy(true);
    setError("");
    try {
      if (annotationsChanged) await api<Repo>("edit", { id: repo.id, tags: normalizedTags, note, pinned,
        ...(summaryChanged && { summary }) });
      else if (summaryChanged) await api("edit", { id: repo.id, summary });
      saved();
    } catch (e) { setError(message(e)); setBusy(false); }
  }

  return <EuiFlyout onClose={dismiss} size="s" aria-labelledby="detail-title" ownFocus>
    <EuiFlyoutHeader hasBorder><span className="section-label">{t("仓库详情")}</span>
      <h2 id="detail-title">{repo.full_name}</h2></EuiFlyoutHeader>
    <EuiFlyoutBody>
      <p className="detail-description">{repo.description || t("仓库未提供简介。")}</p>
      <div className="metadata">{repo.language && <Language name={repo.language} />}
        <span><Icon type="starEmpty" />{repo.stargazers_count.toLocaleString()} Stars</span>
        {repo.archived && <span>{t("已归档")}</span>}</div>
      {repo.topics.length > 0 && <div className="chips">{repo.topics.map(t => <span key={t}>{t}</span>)}</div>}
      <dl className="detail-facts"><dt>{t("收藏日期")}</dt><dd>{date(repo.starred_at)}</dd>
        {url && <><dt>{t("项目地址")}</dt><dd><a href={url} target="_blank" rel="noreferrer">{t("在 GitHub 查看")}<Icon type="popout" /></a></dd></>}
      </dl>
      <form id="edit-form" onSubmit={submit} className="edit-form">
        <h3>{t("收藏整理")}</h3>
        <EuiFormRow label={t("我的标签")} helpText={t("多个标签用逗号分隔。")} fullWidth>
          <EuiFieldText value={tags} onChange={e => setTags(e.target.value)} fullWidth />
        </EuiFormRow>
        <EuiFormRow label={t("备注")} fullWidth>
          <EuiTextArea rows={6} value={note} onChange={e => setNote(e.target.value)} fullWidth placeholder={t("记录收藏原因、使用场景或待办…")} />
        </EuiFormRow>
        <section className="repository-assistant">
          <div className="repository-assistant-heading"><h3>{t("仓库总结")}</h3>
            <EuiButton size="s" isLoading={generating} isDisabled={busy || generating} onClick={generate}>{t("生成总结草稿")}</EuiButton></div>
          <EuiFormRow label={t("总结内容")} helpText={t("可手动修改。生成时会将仓库简介和 README 摘录发给聊天服务，保存后加入语义索引。")} fullWidth>
            <EuiTextArea fullWidth rows={5} maxLength={4096} value={summary} disabled={generating || busy} onChange={event => setSummary(event.target.value)} />
          </EuiFormRow>
          {summaryError && <Failure error={summaryError} />}
        </section>
        <EuiCheckbox id="pin-repository" label={t("置顶这个仓库")} checked={pinned} onChange={e => setPinned(e.target.checked)} />
        {error && <Failure error={error} />}
      </form>
      <RepositoryConversation id={repo.id} />
      <RepositoryCommits id={repo.id} url={url} />
    </EuiFlyoutBody>
    <EuiFlyoutFooter><div className="drawer-actions"><EuiButton onClick={dismiss} isDisabled={busy}>{t("取消")}</EuiButton>
      <EuiButton fill form="edit-form" type="submit" isDisabled={!changed || generating} isLoading={busy}>{t("保存更改")}</EuiButton></div></EuiFlyoutFooter>
  </EuiFlyout>;
}
