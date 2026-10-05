import { useEffect, useId, useState } from "react";
import { IconCheck, IconCopy, IconWorldSearch } from "@tabler/icons-react";
import { toast } from "@heroui/react";

import { invoke } from "./api";
import { errorText } from "./appUtils";
import { Badge, Button, Input, Label, NumberInput, PasswordInput, Switch, Tooltip } from "./components/ui";
import { SettingsPageHeader } from "./SettingsPageHeader";

/** 与后端 `remote_gateway_status` 回传结构一致。 */
type RemoteGatewayFrp = {
  enabled: boolean;
  serverAddr: string;
  serverPort: number;
  token: string;
  remotePort: number;
  binary: string;
  state: string | null;
  message: string | null;
  endpoint: string | null;
  logPath: string | null;
};

type RemoteGatewayStatus = {
  enabled: boolean;
  port: number;
  token: string;
  url: string | null;
  active: boolean;
  frp: RemoteGatewayFrp;
};

const MIN_PORT = 1024;
const MAX_PORT = 65535;
const DEFAULT_PORT = 8799;
const DEFAULT_FRP_SERVER_PORT = 7000;

export function RemoteControlCard() {
  const controlId = useId();
  const portId = controlId + "-port";
  const tokenId = controlId + "-token";
  const urlId = controlId + "-url";
  const frpAddrId = controlId + "-frp-addr";
  const frpServerPortId = controlId + "-frp-server-port";
  const frpTokenId = controlId + "-frp-token";
  const frpRemotePortId = controlId + "-frp-remote-port";
  const frpBinaryId = controlId + "-frp-binary";
  const [status, setStatus] = useState<RemoteGatewayStatus | null>(null);
  const [enabled, setEnabled] = useState(false);
  const [port, setPort] = useState(DEFAULT_PORT);
  const [frpEnabled, setFrpEnabled] = useState(false);
  const [frpServerAddr, setFrpServerAddr] = useState("");
  const [frpServerPort, setFrpServerPort] = useState(DEFAULT_FRP_SERVER_PORT);
  const [frpToken, setFrpToken] = useState("");
  const [frpRemotePort, setFrpRemotePort] = useState(MIN_PORT);
  const [frpBinary, setFrpBinary] = useState("");
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [regenerating, setRegenerating] = useState(false);
  const [unavailable, setUnavailable] = useState<string | null>(null);
  const [copied, setCopied] = useState<"token" | "url" | null>(null);

  const applyStatus = (next: RemoteGatewayStatus) => {
    setStatus(next);
    setEnabled(next.enabled);
    setPort(next.port);
    const frp = next.frp;
    setFrpEnabled(frp.enabled);
    setFrpServerAddr(frp.serverAddr);
    setFrpServerPort(frp.serverPort || DEFAULT_FRP_SERVER_PORT);
    setFrpToken(frp.token);
    setFrpRemotePort(frp.remotePort || MIN_PORT);
    setFrpBinary(frp.binary);
  };

  useEffect(() => {
    let cancelled = false;
    invoke<RemoteGatewayStatus>("remote_gateway_status")
      .then((next) => {
        if (!cancelled) applyStatus(next);
      })
      .catch((error) => {
        if (!cancelled) setUnavailable(errorText(error));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const busy = loading || saving || regenerating;
  const dirty =
    status !== null &&
    (enabled !== status.enabled ||
      port !== status.port ||
      frpEnabled !== status.frp.enabled ||
      frpServerAddr !== status.frp.serverAddr ||
      frpServerPort !== status.frp.serverPort ||
      frpToken !== status.frp.token ||
      frpRemotePort !== status.frp.remotePort ||
      frpBinary !== status.frp.binary);

  const handleSave = async () => {
    setSaving(true);
    try {
      const next = await invoke<RemoteGatewayStatus>("save_remote_gateway_config", {
        enabled,
        port,
        frpEnabled,
        frpServerAddr,
        frpServerPort,
        frpToken,
        frpRemotePort,
        frpBinary,
      });
      applyStatus(next);
      toast.success("已保存，重启 Codex 后生效");
    } catch (error) {
      toast.danger(errorText(error));
    } finally {
      setSaving(false);
    }
  };

  const handleRegenerate = async () => {
    setRegenerating(true);
    try {
      const next = await invoke<RemoteGatewayStatus>("regenerate_remote_gateway_token", {});
      applyStatus(next);
      toast.success("已生成新的访问密钥");
    } catch (error) {
      toast.danger(errorText(error));
    } finally {
      setRegenerating(false);
    }
  };

  const handleCopy = async (kind: "token" | "url", value: string) => {
    if (!value) return;
    try {
      await navigator.clipboard.writeText(value);
      setCopied(kind);
      setTimeout(() => setCopied((current) => (current === kind ? null : current)), 2000);
    } catch {
      toast.danger("复制失败，请手动选择复制");
    }
  };

  const active = status?.active === true;
  const frpState = status?.frp.state ?? null;
  const frpBadge = (() => {
    if (!frpEnabled) return <Badge variant="secondary">未启用</Badge>;
    switch (frpState) {
      case "running":
        return <Badge variant="success">已生效</Badge>;
      case "starting":
        return <Badge variant="warning">启动中</Badge>;
      case "failed":
      case "invalid":
      case "exited":
        return <Badge variant="destructive">映射失败</Badge>;
      case "unknown":
        return <Badge variant="warning">状态未知</Badge>;
      default:
        return <Badge variant="warning">重启后生效</Badge>;
    }
  })();
  const copyIcon = (kind: "token" | "url") =>
    copied === kind ? (
      <IconCheck size={13} className="text-success" aria-hidden="true" />
    ) : (
      <IconCopy size={13} aria-hidden="true" />
    );

  return (
    <section className="secondary-section" aria-labelledby="remote-control-title">
      <div className="prompt-optimization-settings">
        <SettingsPageHeader
          id="remote-control-title"
          title="远程控制"
          icon={<IconWorldSearch size={15} />}
          description="用局域网网页查看并继续 Codex 会话，可切换模型与发送消息。"
          actions={
            <Switch
              checked={enabled}
              disabled={busy || unavailable !== null}
              aria-label="启用远程控制"
              onCheckedChange={setEnabled}
            />
          }
        />
        <div className="module-card-body prompt-optimization-body">
          {unavailable ? (
            <small className="field-hint">{unavailable}</small>
          ) : !enabled ? (
            <small className="field-hint">
              开启并保存后，重启 Codex 即可通过局域网网页远程查看与发送消息。
            </small>
          ) : (
            <div className="prompt-optimization-content">
              <div className="prompt-form-group">
                <div className="prompt-field">
                  <div className="prompt-field-label-row">
                    <Label htmlFor={portId} className="prompt-field-label">
                      监听端口
                    </Label>
                    {active ? (
                      <Badge variant="success">已生效</Badge>
                    ) : (
                      <Badge variant="warning">重启后生效</Badge>
                    )}
                  </div>
                  <div className="prompt-field-control">
                    <NumberInput
                      value={port}
                      minValue={MIN_PORT}
                      maxValue={MAX_PORT}
                      disabled={busy}
                      onChange={setPort}
                      aria-label="监听端口"
                    />
                    <small className="field-hint">
                      局域网内通过该端口访问，范围 {MIN_PORT}-{MAX_PORT}。
                    </small>
                  </div>
                </div>

                <div className="prompt-field">
                  <Label htmlFor={tokenId} className="prompt-field-label">
                    访问密钥
                  </Label>
                  <div className="prompt-field-control">
                    <div className="flex items-center gap-2">
                      <PasswordInput
                        id={tokenId}
                        value={status?.token ?? ""}
                        readOnly
                        disabled={loading}
                        aria-label="访问密钥"
                      />
                      <Tooltip content={copied === "token" ? "已复制到剪贴板" : "复制访问密钥"}>
                        <Button
                          variant="ghost"
                          size="icon-sm"
                          className="shrink-0 text-[var(--codey-muted)] hover:text-[var(--codey-text)]"
                          aria-label="复制访问密钥"
                          disabled={!status?.token}
                          onClick={() => void handleCopy("token", status?.token ?? "")}
                        >
                          {copyIcon("token")}
                        </Button>
                      </Tooltip>
                      <Button
                        variant="light"
                        size="xs"
                        loading={regenerating}
                        disabled={busy}
                        onClick={() => void handleRegenerate()}
                      >
                        重新生成
                      </Button>
                    </div>
                    <small className="field-hint">
                      网页需携带该密钥才能访问；重新生成后需重启 Codex 生效。
                    </small>
                  </div>
                </div>

                <div className="prompt-field">
                  <Label htmlFor={urlId} className="prompt-field-label">
                    访问地址
                  </Label>
                  <div className="prompt-field-control">
                    <div className="flex items-center gap-2">
                      <Input
                        id={urlId}
                        value={status?.url ?? ""}
                        readOnly
                        placeholder="保存并重启 Codex 后生成"
                        aria-label="访问地址"
                      />
                      <Tooltip content={copied === "url" ? "已复制到剪贴板" : "复制访问地址"}>
                        <Button
                          variant="ghost"
                          size="icon-sm"
                          className="shrink-0 text-[var(--codey-muted)] hover:text-[var(--codey-text)]"
                          aria-label="复制访问地址"
                          disabled={!status?.url}
                          onClick={() => void handleCopy("url", status?.url ?? "")}
                        >
                          {copyIcon("url")}
                        </Button>
                      </Tooltip>
                    </div>
                    <small className="field-hint">
                      在手机或另一台电脑的浏览器打开该地址即可查看并继续会话。
                    </small>
                  </div>
                </div>

                <div className="prompt-field">
                  <div className="prompt-field-label-row">
                    <Label className="prompt-field-label">公网映射（frp）</Label>
                    {frpBadge}
                  </div>
                  <div className="prompt-field-control">
                    <div className="flex items-center justify-between gap-3">
                      <small className="field-hint">
                        用 frp 把局域网端口映射到公网，异地也能访问同一页面。
                      </small>
                      <Switch
                        checked={frpEnabled}
                        disabled={busy}
                        aria-label="启用公网映射"
                        onCheckedChange={setFrpEnabled}
                      />
                    </div>
                  </div>
                </div>

                {frpEnabled ? (
                  <>
                    <div className="prompt-field">
                      <Label htmlFor={frpAddrId} className="prompt-field-label">
                        frp 服务器地址
                      </Label>
                      <div className="prompt-field-control">
                        <Input
                          id={frpAddrId}
                          value={frpServerAddr}
                          disabled={busy}
                          placeholder="frps.example.com"
                          onChange={(event) => setFrpServerAddr(event.target.value)}
                          aria-label="frp 服务器地址"
                        />
                      </div>
                    </div>

                    <div className="prompt-field">
                      <Label htmlFor={frpServerPortId} className="prompt-field-label">
                        frp 服务器端口
                      </Label>
                      <div className="prompt-field-control">
                        <NumberInput
                          value={frpServerPort}
                          minValue={MIN_PORT}
                          maxValue={MAX_PORT}
                          disabled={busy}
                          onChange={setFrpServerPort}
                          aria-label="frp 服务器端口"
                        />
                        <small className="field-hint">frps 的监听端口，默认 {DEFAULT_FRP_SERVER_PORT}。</small>
                      </div>
                    </div>

                    <div className="prompt-field">
                      <Label htmlFor={frpTokenId} className="prompt-field-label">
                        认证令牌
                      </Label>
                      <div className="prompt-field-control">
                        <PasswordInput
                          id={frpTokenId}
                          value={frpToken}
                          disabled={busy}
                          placeholder="与 frps 的 auth.token 一致，可留空"
                          onChange={(event) => setFrpToken(event.target.value)}
                          aria-label="frp 认证令牌"
                        />
                      </div>
                    </div>

                    <div className="prompt-field">
                      <Label htmlFor={frpRemotePortId} className="prompt-field-label">
                        远程端口
                      </Label>
                      <div className="prompt-field-control">
                        <NumberInput
                          value={frpRemotePort}
                          minValue={MIN_PORT}
                          maxValue={MAX_PORT}
                          disabled={busy}
                          onChange={setFrpRemotePort}
                          aria-label="远程端口"
                        />
                        <small className="field-hint">frps 上对外开放的端口，需在服务端放行。</small>
                      </div>
                    </div>

                    <div className="prompt-field">
                      <Label htmlFor={frpBinaryId} className="prompt-field-label">
                        本地 frpc 路径
                      </Label>
                      <div className="prompt-field-control">
                        <Input
                          id={frpBinaryId}
                          value={frpBinary}
                          disabled={busy}
                          placeholder="留空则自动下载 frpc"
                          onChange={(event) => setFrpBinary(event.target.value)}
                          aria-label="本地 frpc 路径"
                        />
                      </div>
                    </div>

                    {status?.frp.endpoint ? (
                      <small className="field-hint">映射地址：{status.frp.endpoint}</small>
                    ) : null}
                    {status?.frp.message ? (
                      <small className="field-hint">{status.frp.message}</small>
                    ) : null}
                    {status?.frp.logPath ? (
                      <small className="field-hint">frpc 日志：{status.frp.logPath}</small>
                    ) : null}
                  </>
                ) : null}
              </div>

              <div className="prompt-optimization-toolbar-actions">
                <Button
                  variant="filled"
                  size="sm"
                  loading={saving}
                  disabled={busy || !dirty}
                  onClick={() => void handleSave()}
                >
                  保存
                </Button>
              </div>
            </div>
          )}
        </div>
      </div>
    </section>
  );
}
