import { Empty, Select, Typography } from "antd";
import { useCallback, useEffect, useImperativeHandle, useRef, useState, type Ref } from "react";
import type { Recording } from "./api";
import type { RecordingPosition } from "./reviewLogic";

export interface RecordingControls {
  seek: (position: RecordingPosition) => void;
}

type Cue = RecordingPosition & { play: boolean };

export default function RecordingPlayer({ recordings, ref }: { recordings: Recording[]; ref: Ref<RecordingControls> }) {
  const section = useRef<HTMLElement>(null);
  const video = useRef<HTMLVideoElement>(null);
  const appliedCue = useRef<Cue | null>(null);
  const [cue, setCue] = useState<Cue | null>(null);
  const [failedUrl, setFailedUrl] = useState<string | null>(null);
  const ordered = [...recordings].sort((left, right) => left.seq - right.seq);
  const active = Math.max(ordered.findIndex((segment) => segment.seq === cue?.seq), 0);
  const seq = ordered[active]?.seq;
  const url = ordered[active]?.url;

  const applyCue = useCallback((element: HTMLVideoElement) => {
    if (!cue || !url || cue.seq !== seq || appliedCue.current === cue || element.readyState < HTMLMediaElement.HAVE_METADATA) return;
    if (element.currentSrc !== new URL(url, document.baseURI).href) return;
    element.currentTime = Number.isFinite(element.duration) ? Math.min(cue.seconds, element.duration) : cue.seconds;
    appliedCue.current = cue;
    if (cue.play) void element.play().catch(() => {});
  }, [cue, seq, url]);

  useEffect(() => {
    if (video.current) applyCue(video.current);
  }, [applyCue]);

  useImperativeHandle(ref, () => ({
    seek(position) {
      setCue({ ...position, play: video.current?.paused === false });
      section.current?.scrollIntoView({ block: "start" });
    },
  }), []);

  return (
    <section className="surface run-panel recording-panel" ref={section} aria-label="录屏">
      <div className="panel-heading">
        <Typography.Title level={4}>录屏</Typography.Title>
        {!!url && (
          <Select size="small" aria-label="录屏分段" value={seq}
            options={ordered.map((segment, index) => ({ value: segment.seq, label: `第 ${index + 1} / ${ordered.length} 段` }))}
            onChange={(next: number) => setCue({ seq: next, seconds: 0, play: video.current?.paused === false })}
          />
        )}
      </div>
      {url ? (
        <>
          <video className="recording-video" aria-label="执行录屏" ref={video} src={url} controls playsInline preload="metadata"
            onLoadedMetadata={(event) => {
              setFailedUrl(null);
              applyCue(event.currentTarget);
            }}
            onEnded={() => {
              const next = ordered[active + 1];
              if (next) setCue({ seq: next.seq, seconds: 0, play: true });
            }}
            onError={() => setFailedUrl(url)}
          />
          {failedUrl === url && <Typography.Text type="danger">录屏加载失败，请稍后重试或选择其他分段。</Typography.Text>}
        </>
      ) : <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="本次执行没有录屏" />}
    </section>
  );
}
