import { useState, type FormEvent } from "react";
import {
  EuiButton, EuiButtonEmpty, EuiFieldText, EuiFormRow,
  EuiModal, EuiModalBody, EuiModalFooter, EuiModalHeader, EuiModalHeaderTitle,
} from "@elastic/eui";
import { api, message } from "./api";
import { t } from "./locale";
import { Failure } from "./shared";

export interface Profile { username: string; admin: boolean; nickname: string | null }

function Avatar({ username, name }: { username: string; name: string }) {
  const characters = Array.from(name);
  const first = characters[0] ?? "";
  const marks = /^\p{Script=Han}$/u.test(first) ? first : characters.slice(0, 2).join("").toLocaleUpperCase();
  const colors = ["#734b37", "#4e6457", "#53647e", "#77526c", "#785d39"];
  const hash = Array.from(username).reduce((value, char) => (value * 31 + char.codePointAt(0)!) >>> 0, 0);
  return <span className="profile-avatar" aria-hidden="true" style={{ backgroundColor: colors[hash % colors.length] }}>{marks}</span>;
}

export function ProfileButton({ profile, changed }: { profile: Profile; changed: (profile: Profile) => void }) {
  const name = profile.nickname || profile.username;
  const [open, setOpen] = useState(false);
  const [nickname, setNickname] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const dirty = nickname.trim() !== (profile.nickname || "");
  async function save(event: FormEvent) {
    event.preventDefault();
    if (!dirty || busy) return;
    setBusy(true); setError("");
    try {
      changed(await api<Profile>("me", { nickname: nickname.trim() }));
      setOpen(false);
    } catch (e) { setError(message(e)); } finally { setBusy(false); }
  }
  return <>
    <button type="button" className="profile-button" aria-label={t("编辑个人资料")} title={t("编辑个人资料")}
      onClick={() => { setNickname(profile.nickname || ""); setError(""); setOpen(true); }}>
      <Avatar username={profile.username} name={name} /><span className="profile-name">{name}</span>
    </button>
    {open && <EuiModal aria-labelledby="profile-title" onClose={() => { if (!busy) setOpen(false); }}>
      <EuiModalHeader><EuiModalHeaderTitle id="profile-title">{t("个人资料")}</EuiModalHeaderTitle></EuiModalHeader>
      <EuiModalBody>
        <form id="profile-form" onSubmit={save}>
          <div className="profile-preview"><Avatar username={profile.username} name={nickname.trim() || profile.username} />
            <span>{t("登录用户名：{0}", profile.username)}</span></div>
          <EuiFormRow label={t("昵称")} helpText={t("留空时显示登录用户名。头像会自动生成。")} fullWidth>
            <EuiFieldText value={nickname} onChange={event => setNickname(event.target.value)} maxLength={48} fullWidth disabled={busy} />
          </EuiFormRow>
          {error && <Failure error={error} />}
        </form>
      </EuiModalBody>
      <EuiModalFooter>
        <EuiButtonEmpty onClick={() => setOpen(false)} isDisabled={busy}>{t("取消")}</EuiButtonEmpty>
        <EuiButton type="submit" form="profile-form" fill isLoading={busy} isDisabled={!dirty}>{t("保存变更")}</EuiButton>
      </EuiModalFooter>
    </EuiModal>}
  </>;
}
