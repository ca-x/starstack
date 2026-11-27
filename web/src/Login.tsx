import { t } from "./locale";
import { PreferenceMenu } from "./Preferences";
import logo from "./assets/logo-display.webp";
import { useEffect, useState, type FormEvent } from "react";
import { EuiButton, EuiFieldPassword, EuiFieldText, EuiFormRow } from "@elastic/eui";
import { api, request, message } from "./api";
import { Failure } from "./shared";

export function Login() {
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  const [setup, setSetup] = useState<boolean>();
  const [revision, setRevision] = useState(0);
  const [confirmation, setConfirmation] = useState("");
  const validPassword = new TextEncoder().encode(password).length >= 8 && new TextEncoder().encode(password).length <= 1024;
  useEffect(() => {
    const controller = new AbortController();
    api<{ required: boolean }>("setup", undefined, controller.signal)
      .then(result => setSetup(result.required))
      .catch(e => { if (!controller.signal.aborted) setError(message(e)); });
    return () => controller.abort();
  }, [revision]);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy || setup === undefined || (setup && (!validPassword || password !== confirmation))) return;
    setBusy(true);
    setError("");
    try {
      const credentials = { username: username.trim(), password };
      if (setup) {
        await request("setup", credentials);
        setSetup(false);
      }
      await request("login", credentials);
      location.replace("/admin");
    } catch (e) { setError(message(e)); setBusy(false); }
  }

  return <main className="login-layout"><div className="login-preferences"><PreferenceMenu /></div><form className="login-panel" onSubmit={submit}>
      <img src={logo} className="login-logo" width="96" height="96" alt="" />
      <h1>StarStack</h1>
      <p className="login-caption">{t(setup === undefined ? "正在检查初始化状态…" : setup ? "创建首个管理员" : "登录星栈")}</p>
      {setup === undefined ? error && <Failure error={error} retry={() => { setError(""); setRevision(n => n + 1); }} /> : <>
      <EuiFormRow label={t("用户名")} fullWidth><EuiFieldText autoComplete="username" name="username" required fullWidth
        value={username} onChange={e => setUsername(e.target.value)} /></EuiFormRow>
      <EuiFormRow label={t("密码")} helpText={setup ? t("密码长度应为 8 至 1024 字节") : undefined} fullWidth><EuiFieldPassword autoComplete={setup ? "new-password" : "current-password"} name="password" required fullWidth
        type="dual" value={password} onChange={e => setPassword(e.target.value)} /></EuiFormRow>
      {setup && <EuiFormRow label={t("确认密码")} fullWidth isInvalid={!!confirmation && password !== confirmation} error={t("两次输入的密码不一致。")}>
        <EuiFieldPassword autoComplete="new-password" required fullWidth type="dual" value={confirmation} onChange={e => setConfirmation(e.target.value)} />
      </EuiFormRow>}
      {error && <Failure error={error} />}
      <EuiButton type="submit" fill fullWidth isLoading={busy} isDisabled={!username.trim() || !password || !!setup && (!validPassword || password !== confirmation)}>{t(setup ? "创建管理员并登录" : "登录")}</EuiButton></>}
    </form>
  </main>;
}
