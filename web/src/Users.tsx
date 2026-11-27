import { t } from "./locale";
import { useEffect, useState, type FormEvent } from "react";
import {
  EuiButton, EuiButtonEmpty, EuiCheckbox, EuiConfirmModal, EuiFieldPassword, EuiFieldText,
  EuiFlyout, EuiFlyoutBody, EuiFlyoutFooter, EuiFlyoutHeader, EuiFormRow,
} from "@elastic/eui";
import { api, message } from "./api";
import { Failure } from "./shared";

interface User { username: string; admin: boolean; disabled: boolean; primary: boolean }

function UserForm({ user, canDelete, close, saved }: { user?: User; canDelete: boolean; close: () => void; saved: (deleted?: boolean) => void }) {
  const [username, setUsername] = useState(user?.username || "");
  const [password, setPassword] = useState("");
  const [admin, setAdmin] = useState(user?.admin || false);
  const [disabled, setDisabled] = useState(user?.disabled || false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [confirm, setConfirm] = useState(false);
  const changed = user ? !!password || admin !== user.admin || disabled !== user.disabled : !!username.trim() && !!password;
  async function submit(e: FormEvent) {
    e.preventDefault();
    if (busy || !changed) return;
    setBusy(true); setError("");
    try {
      if (user) await api("users/update", { username: user.username, ...(password && { password }),
        ...(admin !== user.admin && { admin }), ...(disabled !== user.disabled && { disabled }) });
      else await api("users", { username: username.trim(), password, admin });
      saved();
    } catch (e) { setError(message(e)); setBusy(false); }
  }
  async function remove() {
    if (busy || !user || !canDelete) return;
    setBusy(true); setError("");
    try {
      await api("users/delete", { username: user.username });
      saved(true);
    } catch (e) { setError(message(e)); setConfirm(false); setBusy(false); }
  }
  return <><EuiFlyout onClose={() => { if (!busy) close(); }} size="s" aria-labelledby="user-title" ownFocus>
    <EuiFlyoutHeader hasBorder><h2 id="user-title">{user ? t("管理 {0}", user.username) : t("创建用户")}</h2></EuiFlyoutHeader>
    <EuiFlyoutBody><form className="user-form" id="user-form" onSubmit={submit}>
      {!user && <EuiFormRow label={t("用户名")} fullWidth><EuiFieldText fullWidth autoComplete="off" required value={username} onChange={e => setUsername(e.target.value)} /></EuiFormRow>}
      <EuiFormRow label={user ? t("重置密码") : t("初始密码")} helpText={user ? t("留空可保留现有密码。") : undefined} fullWidth>
        <EuiFieldPassword fullWidth type="dual" autoComplete="new-password" required={!user} value={password} onChange={e => setPassword(e.target.value)} />
      </EuiFormRow>
      <EuiCheckbox id="role-admin" label={t("管理员")} checked={admin} disabled={user?.primary} onChange={e => setAdmin(e.target.checked)} />
      {user && <EuiCheckbox id="user-disabled" label={t("停用账号")} checked={disabled} disabled={user.primary} onChange={e => setDisabled(e.target.checked)} />}
      {user?.primary && <p className="subdued">{t("初始管理员的角色和启用状态由系统固定。")}</p>}
      {user && <p className="user-impact">{t("保存变更将清除该用户的登录会话及 MCP 令牌。")}</p>}
      {error && <Failure error={error} />}
    </form></EuiFlyoutBody>
    <EuiFlyoutFooter><div className="drawer-actions">
      {canDelete && <EuiButtonEmpty color="danger" isDisabled={busy} onClick={() => setConfirm(true)}>{t("删除用户")}</EuiButtonEmpty>}<EuiButton isDisabled={busy} onClick={close}>{t("取消")}</EuiButton>
      <EuiButton type="submit" form="user-form" fill isLoading={busy} isDisabled={!changed}>{user ? t("保存变更") : t("创建")}</EuiButton></div></EuiFlyoutFooter>
  </EuiFlyout>
    {confirm && user && <EuiConfirmModal title={t("删除账号 {0}？", user.username)}
      onCancel={() => { if (!busy) setConfirm(false); }} onConfirm={remove}
      cancelButtonText={t("取消")} confirmButtonText={t("删除用户")} buttonColor="danger" isLoading={busy}>
      <p>{t("账号、登录会话和令牌将失效。收藏和批注仍保留在数据库中，无法通过该账号访问。")}</p>
    </EuiConfirmModal>}
  </>;
}

export function Users({ username }: { username: string }) {
  const [users, setUsers] = useState<User[]>([]);
  const [editing, setEditing] = useState<User | "new" | null>(null);
  const [revision, setRevision] = useState(0);
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState("");
  const [status, setStatus] = useState("");
  useEffect(() => {
    const controller = new AbortController();
    setError("");
    api<{ users: User[] }>("users", undefined, controller.signal)
      .then(data => { if (!controller.signal.aborted) { setUsers(data.users); setLoaded(true); } })
      .catch(e => { if (!controller.signal.aborted) setError(message(e)); });
    return () => controller.abort();
  }, [revision]);
  return <>
    <div className="page-heading"><div><h1>{t("用户管理")}</h1><p>{t("创建账号、调整角色及管理访问权限。")}</p></div></div>
    <section className="collection"><div className="result-head"><h2>{t("用户列表")}</h2><EuiButton size="s" fill onClick={() => setEditing("new")}>{t("创建用户")}</EuiButton></div>
      {error ? <Failure error={error} retry={() => setRevision(n => n + 1)} /> : !loaded ? <p className="empty" role="status">{t("正在读取用户…")}</p> :
        <ul className="user-list">{users.map(user => <li key={user.username}><div><strong>{user.username}</strong>
          <small>{user.primary ? t("初始管理员") : user.admin ? t("管理员") : t("普通用户")} · {user.disabled ? t("已停用") : t("已启用")}</small></div>
          <EuiButtonEmpty aria-label={t("管理 {0}", user.username)} onClick={() => { setStatus(""); setEditing(user); }}>{t("管理")}</EuiButtonEmpty></li>)}</ul>}</section>
    {status && <p role="status" className="progress-note">{status}</p>}
    {editing && <UserForm user={editing === "new" ? undefined : editing} canDelete={editing !== "new" && editing.username !== username} close={() => setEditing(null)} saved={deleted => {
      setStatus(deleted ? t("用户已删除。") : editing === "new" ? t("用户已创建。") : t("用户已更新。")); setEditing(null); setRevision(n => n + 1);
    }} />}
  </>;
}
