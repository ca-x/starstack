import { useEffect, useRef, useState, type FormEvent } from "react";
import { EuiAccordion, EuiButton, EuiButtonEmpty, EuiCommentList, EuiFormRow, EuiText, EuiTextArea } from "@elastic/eui";
import { api, message } from "./api";
import { getLocale, t } from "./locale";
import { Failure } from "./shared";

interface Answer { answer: string; source_paths: string[]; code: string; truncated: boolean }
interface Turn extends Answer { question: string }

export function RepositoryConversation({ id }: { id: number }) {
  const [question, setQuestion] = useState("");
  const [turns, setTurns] = useState<Turn[]>([]);
  const [busy, setBusy] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const [indexed, setIndexed] = useState<number>();
  const [status, setStatus] = useState("");
  const request = useRef<AbortController>();
  useEffect(() => () => request.current?.abort(), []);

  async function ask(event: FormEvent) {
    event.preventDefault();
    const text = question.trim();
    if (!text || busy) return;
    const controller = new AbortController();
    request.current = controller;
    setBusy(true); setError(""); setStatus("");
    try {
      const result = await api<Answer>("repository/ask", {
        id, question: text, history: turns.slice(-4).map(({ question, answer }) => ({ question, answer: Array.from(answer).slice(0, 4000).join("") })),
      }, controller.signal);
      if (controller.signal.aborted) return;
      setTurns(current => [...current, { question: text, ...result }]); setQuestion("");
    } catch (e) {
      if (!controller.signal.aborted) setError(message(e));
    } finally { if (!controller.signal.aborted) setBusy(false); }
  }

  async function saveCode(code: string, turn?: number) {
    if (saving) return;
    setSaving(true); setError(""); setStatus("");
    try {
      await api("repository/context", { id, code });
      setIndexed(turn);
      setStatus(t(code ? "已保存代码片段。下次更新索引时可检索。" : "已移除保存的代码片段。"));
    } catch (e) { setError(message(e)); } finally { setSaving(false); }
  }

  return <section className="repository-assistant">
    <div><h3>{t("与仓库代码对话")}</h3><p>{t("提问时会读取 GitHub 上的相关源文件，并将片段、README 摘录和当前对话发送给配置的聊天服务。")}</p></div>
    {turns.length > 0 && <EuiCommentList aria-label={t("仓库对话")} gutterSize="m" comments={turns.flatMap((turn, index) => [
      { username: t("你"), timelineAvatar: "user", children: <EuiText size="s"><p>{turn.question}</p></EuiText> },
      { username: "StarStack", timelineAvatar: "sparkles", children: <div className="repository-assistant">
        <EuiText size="s" className="repository-answer"><p>{turn.answer}</p></EuiText>
        <EuiAccordion id={`repo-${id}-sources-${index}`} buttonContent={t("参考文件（{0}）", turn.source_paths.length)} paddingSize="s">
          <ul>{turn.source_paths.map(path => <li key={path}><code>{path}</code></li>)}</ul>
          {turn.truncated && <p>{t("GitHub 只返回了部分文件清单。")}</p>}
        </EuiAccordion>
        {index === turns.length - 1 && <EuiButtonEmpty size="s" isDisabled={saving || indexed === index} onClick={() => saveCode(turn.code, index)}>
          {t("保存这些代码片段用于检索")}</EuiButtonEmpty>}
      </div> },
    ])} />}
    <form onSubmit={ask} className="repository-assistant">
      <EuiFormRow label={t(turns.length ? "追问" : "关于这个仓库的问题")} fullWidth>
        <EuiTextArea rows={2} fullWidth maxLength={1024} value={question} disabled={busy}
          onChange={event => setQuestion(event.target.value)} placeholder={t("例如：它在哪里初始化索引？")} />
      </EuiFormRow>
      <div className="row-controls">
        <EuiButton size="s" type="submit" isLoading={busy} isDisabled={!question.trim() || busy}>{t("发送问题")}</EuiButton>
        {turns.length > 0 && <EuiButtonEmpty size="s" isDisabled={busy || saving} onClick={() => { setTurns([]); setIndexed(undefined); setStatus(""); setError(""); }}>{t("清空对话")}</EuiButtonEmpty>}
        <EuiButtonEmpty size="s" isDisabled={saving} onClick={() => saveCode("")}>{t("移除已保存代码片段")}</EuiButtonEmpty>
      </div>
    </form>
    <p>{t("保存会替换该仓库此前保存的代码片段。使用远端 Embedding 时，索引会将这些片段发给该服务。")}</p>
    {status && <p role="status">{status}</p>}
    {error && <Failure error={error} />}
  </section>;
}

export async function draftSummary(id: number) {
  return (await api<{ summary: string }>("repository/summary", { id, language: getLocale() })).summary;
}
