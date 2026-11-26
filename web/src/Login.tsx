import { t } from "./locale";
import { PreferenceMenu } from "./Preferences";
import logo from "./assets/logo-display.webp";
import { useState, type FormEvent } from "react";
import { EuiButton, EuiFieldPassword, EuiFieldText, EuiFormRow } from "@elastic/eui";
import { request, message } from "./api";
import { Failure } from "./shared";

export function Login() {
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError("");
    try {
      await request("login", { username: username.trim(), password });
      location.replace("/admin");
    } catch (e) { setError(message(e)); setBusy(false); }
  }

  return <main className="login-layout"><div className="login-preferences"><PreferenceMenu /></div><form className="login-panel" onSubmit={submit}>
      <img src={logo} className="login-logo" width="96" height="96" alt="" />
      <h1>StarStack</h1>
      <p className="login-caption">{t("管理员登录")}</p>
      <EuiFormRow label={t("用户名")} fullWidth><EuiFieldText autoComplete="username" name="username" required fullWidth
        value={username} onChange={e => setUsername(e.target.value)} /></EuiFormRow>
      <EuiFormRow label={t("密码")} fullWidth><EuiFieldPassword autoComplete="current-password" name="password" required fullWidth
        type="dual" value={password} onChange={e => setPassword(e.target.value)} /></EuiFormRow>
      {error && <Failure error={error} />}
      <EuiButton type="submit" fill fullWidth isLoading={busy} disabled={!username.trim() || !password}>{t("登录")}</EuiButton>
    </form>
  </main>;
}
