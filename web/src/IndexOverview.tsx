import { useEffect, useState } from "react";
import { api, date, message } from "./api";
import { getLocale, t } from "./locale";
import { formatBytes } from "./ModelRow";
import { Failure } from "./shared";
import { WaterLevel } from "./WaterLevel";

interface Inventory {
  state: "unbuilt" | "pending" | "ready" | "needs_attention";
  body_pending?: boolean;
  repositories: number;
  indexed: number;
  checkpoints: number;
  projection_bytes: number;
  vector_payload_bytes: number;
  legacy_payload_bytes: number;
  last_published_at: string | null;
}

const states = {
  unbuilt: "尚未建立索引", pending: "等待更新", ready: "索引已就绪", needs_attention: "索引需要处理",
} as const;

export function IndexOverview({ visible, working, attention, modelBytes }: {
  visible: boolean; working: boolean; attention: boolean; modelBytes?: number;
}) {
  const [inventory, setInventory] = useState<Inventory>();
  const [error, setError] = useState("");
  const [revision, setRevision] = useState(0);

  useEffect(() => {
    if (!visible) return;
    const controller = new AbortController();
    let timer: number;
    async function load() {
      try {
        const result = await api<Inventory>("index/status", undefined, controller.signal);
        if (!controller.signal.aborted) { setInventory(result); setError(""); }
      } catch (e) {
        if (!controller.signal.aborted) setError(message(e));
      }
      if (!controller.signal.aborted) timer = window.setTimeout(load, 8000);
    }
    void load();
    return () => { controller.abort(); window.clearTimeout(timer); };
  }, [visible, working, revision]);

  const numbers = new Intl.NumberFormat(getLocale());
  return <section className="operation index-overview" aria-labelledby="index-overview-title">
    <div className="index-overview-title"><h2 id="index-overview-title">{t("索引状态")}</h2>
      {inventory && <span>{t(working ? "正在更新索引…" : attention ? states.needs_attention : states[inventory.state])}</span>}
    </div>
    {error && <Failure error={error} retry={() => setRevision(n => n + 1)} />}
    {!inventory && !error && <p role="status">{t("正在读取索引状态…")}</p>}
    {inventory && <>
      <div className="index-coverage">
        <WaterLevel value={inventory.indexed} total={inventory.repositories} label={t("已发布索引覆盖率")} />
        <div><strong>{t("{0} / {1} 个仓库已入索引", numbers.format(inventory.indexed), numbers.format(inventory.repositories))}</strong>
          <span>{t("已发布索引覆盖率")}</span></div>
      </div>
      <dl className="index-overview-facts">
        <div><dt>{t("最近更新索引")}</dt><dd>{inventory.last_published_at ? date(inventory.last_published_at, true) : t("尚未发布")}</dd></div>
        <div><dt>{t("已保存断点")}</dt><dd>{numbers.format(inventory.checkpoints)}</dd></div>
        <div><dt>{t("索引数据占用")}</dt><dd>{formatBytes(inventory.projection_bytes + inventory.vector_payload_bytes + inventory.legacy_payload_bytes)}</dd></div>
        {modelBytes !== undefined && <div><dt>{t("当前模型文件")}</dt><dd>{formatBytes(modelBytes)}</dd></div>}
      </dl>
      {inventory.body_pending && <p role="status">{t("元数据已可检索，正文索引待补全。")}</p>}
      {!inventory.last_published_at && inventory.checkpoints > 0 &&
        <p>{t("已计算的向量已保存，可断点接续。首版索引发布后即可进行语义检索。")}</p>}
      <details className="index-storage-details"><summary>{t("查看索引占用明细")}</summary>
        <dl className="index-overview-facts">
          <div><dt>{t("HawDB 检索索引")}</dt><dd>{inventory.projection_bytes > 0 ? formatBytes(inventory.projection_bytes) : t("尚未发布")}</dd></div>
          <div><dt>{t("HawDB 原始向量与断点")}</dt><dd>{formatBytes(inventory.vector_payload_bytes)}</dd></div>
          {inventory.legacy_payload_bytes > 0 && <div><dt>{t("待迁移的旧版向量")}</dt><dd>{formatBytes(inventory.legacy_payload_bytes)}</dd></div>}
        </dl>
      </details>
    </>}
  </section>;
}
