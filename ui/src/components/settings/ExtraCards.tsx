// Smaller server-settings cards used by several Settings sections: AI fallback (settings.llm),
// server/network (settings.server) and backup schedule (settings.backup).

import { useState } from 'react';
import type { LlmSettings, ServerSettings, ServerSettingsSection } from '../../types2';
import { Toggle } from '../Toggle';
import { NumInput, RestartBanner, Row, SaveBar, SettingsGate, useDraft, useServerSettings, type ServerSettingsState } from './serverSettings';

function splitList(s: string): string[] {
  return s
    .split(/[\n,]/)
    .map((x) => x.trim())
    .filter(Boolean);
}

function LlmCard({ state, s }: { state: ServerSettingsState; s: ServerSettings }) {
  const l = useDraft<LlmSettings>(s.llm);
  const [models, setModels] = useState<string | null>(null);
  if (!s.llm) return null;
  const d = l.draft;
  const modelText = models ?? (d.fallback_models ?? []).join('\n');
  return (
    <div className="card">
      <div className="card-title">Last-resort fallback</div>
      <p className="muted card-lead">
        When every provider in a role&apos;s chain fails, SiloForge tries this OpenAI-compatible endpoint with these models, in order.
      </p>
      <div className="settings-list">
        <Row label="Fallback URL" desc="Base URL ending in /v1." wide>
          <input className="input mono-sm" value={d.fallback_url} onChange={(e) => l.set('fallback_url', e.target.value)} aria-label="Fallback URL" placeholder="http://127.0.0.1:3460/v1" />
        </Row>
        <Row label="Fallback models" desc="One per line, tried top to bottom." wide>
          <textarea
            className="input mono-sm"
            rows={3}
            value={modelText}
            aria-label="Fallback models"
            onChange={(e) => {
              setModels(e.target.value);
              l.set('fallback_models', splitList(e.target.value));
            }}
          />
        </Row>
        <Row label="Timeout" desc="Per request.">
          <NumInput label="Timeout" value={d.timeout_ms} onChange={(v) => l.set('timeout_ms', v)} min={1000} max={900000} unit="ms" />
        </Row>
      </div>
      <SaveBar
        dirty={l.dirty}
        saving={state.saving}
        onReset={() => {
          l.reset();
          setModels(null);
        }}
        onSave={() => void state.save({ llm: l.changes as Record<string, unknown> }, 'AI fallback saved').then((ok) => ok && setModels(null))}
      />
    </div>
  );
}

export function LlmFallbackCard() {
  const state = useServerSettings();
  if (state.unavailable || state.forbidden) return null;
  return <SettingsGate state={state}>{(s) => <LlmCard state={state} s={s} />}</SettingsGate>;
}

function ServerCard({ state, s }: { state: ServerSettingsState; s: ServerSettings }) {
  const v = useDraft<ServerSettingsSection>(s.server);
  const [binds, setBinds] = useState<string | null>(null);
  const [cors, setCors] = useState<string | null>(null);
  if (!s.server) return null;
  const d = v.draft;
  return (
    <div className="card">
      <div className="card-title">Server</div>
      <div className="settings-list">
        <Row label="Listen addresses" desc="Where the API and UI listen. Changing this needs a restart." wide>
          <textarea
            className="input mono-sm"
            rows={2}
            value={binds ?? (d.binds ?? []).join('\n')}
            aria-label="Listen addresses"
            onChange={(e) => {
              setBinds(e.target.value);
              v.set('binds', splitList(e.target.value));
            }}
          />
        </Row>
        <Row label="Allowed browser origins" desc="Extra origins allowed to call the API from a browser (CORS). Leave empty for same-origin only." wide>
          <textarea
            className="input mono-sm"
            rows={2}
            value={cors ?? (d.cors_origins ?? []).join('\n')}
            aria-label="Allowed origins"
            onChange={(e) => {
              setCors(e.target.value);
              v.set('cors_origins', splitList(e.target.value));
            }}
          />
        </Row>
        <Row label="Allow sign-up" desc="Let new people create accounts from the login page.">
          <Toggle checked={!!d.allow_registration} onChange={(x) => v.set('allow_registration', x)} label="Allow sign-up" />
        </Row>
      </div>
      <SaveBar
        dirty={v.dirty}
        saving={state.saving}
        onReset={() => {
          v.reset();
          setBinds(null);
          setCors(null);
        }}
        onSave={() =>
          void state.save({ server: v.changes as Record<string, unknown> }, 'Server settings saved').then((ok) => {
            if (ok) {
              setBinds(null);
              setCors(null);
            }
          })
        }
      />
    </div>
  );
}

export function ServerSettingsCard() {
  const state = useServerSettings();
  if (state.unavailable || state.forbidden) return null;
  return (
    <>
      <RestartBanner state={state} />
      <SettingsGate state={state}>{(s) => <ServerCard state={state} s={s} />}</SettingsGate>
    </>
  );
}

type BackupCfg = NonNullable<ServerSettings['backup']>;

function BackupCard({ state, s }: { state: ServerSettingsState; s: ServerSettings }) {
  const b = useDraft<BackupCfg>(s.backup);
  const [pass, setPass] = useState('');
  if (!s.backup) return null;
  const d = b.draft;
  const dirty = b.dirty || pass !== '';
  return (
    <div className="card">
      <div className="card-title">Automatic backups</div>
      <div className="settings-list">
        <Row label="Backup folder" desc="Where scheduled backups are written on the server." wide>
          <input className="input mono-sm" value={d.dir ?? ''} onChange={(e) => b.set('dir', e.target.value)} aria-label="Backup folder" />
        </Row>
        <Row label="Schedule" desc={<>Cron expression (minute hour day month weekday). Default <code>17 3 * * *</code> runs daily at 03:17.</>}>
          <input className="input mono-sm" value={d.schedule ?? ''} onChange={(e) => b.set('schedule', e.target.value)} aria-label="Backup schedule" />
        </Row>
        <Row label="Keep" desc="Older scheduled backups are deleted.">
          <NumInput label="Backups to keep" value={d.keep ?? 10} onChange={(v) => b.set('keep', v)} min={1} max={365} unit="backups" />
        </Row>
        <Row label="Encryption passphrase" desc={d.passphrase_set ? 'A passphrase is saved. Type a new one to replace it.' : 'Without one, backups leave out secrets.env.'}>
          <input className="input" type="password" autoComplete="new-password" value={pass} onChange={(e) => setPass(e.target.value)} placeholder={d.passphrase_set ? '••••••••' : 'none'} aria-label="Backup passphrase" />
        </Row>
      </div>
      <SaveBar
        dirty={dirty}
        saving={state.saving}
        onReset={() => {
          b.reset();
          setPass('');
        }}
        onSave={() => {
          const patch: Record<string, unknown> = { ...b.changes };
          delete patch.passphrase_set;
          if (pass) patch.passphrase = pass;
          void state.save({ backup: patch }, 'Backup settings saved').then((ok) => ok && setPass(''));
        }}
      />
    </div>
  );
}

export function BackupSettingsCard() {
  const state = useServerSettings();
  if (state.unavailable || state.forbidden) return null;
  return <SettingsGate state={state}>{(s) => <BackupCard state={state} s={s} />}</SettingsGate>;
}
