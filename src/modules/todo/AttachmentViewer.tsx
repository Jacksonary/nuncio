/**
 * Full-window image viewer for task attachments.
 *
 * Opened by clicking an inline image preview. Renders through a portal so the
 * overlay covers the whole app (not just the detail pane), and offers the
 * basic operations people expect from an image preview: zoom, rotate, pan and
 * reset.
 */
import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import {
  Add20Regular,
  ArrowReset20Regular,
  ArrowRotateClockwise20Regular,
  ArrowRotateCounterclockwise20Regular,
  Dismiss20Regular,
  Subtract20Regular,
} from "@fluentui/react-icons";
import { useT } from "@/lib/i18n";
import styles from "./AttachmentViewer.module.css";

const MIN_SCALE = 0.2;
const MAX_SCALE = 6;
const STEP = 1.2;

export interface AttachmentViewerProps {
  url: string;
  name: string;
  onClose: () => void;
}

export function AttachmentViewer({ url, name, onClose }: AttachmentViewerProps) {
  const t = useT();
  const [scale, setScale] = useState(1);
  const [rotation, setRotation] = useState(0);
  const [offset, setOffset] = useState({ x: 0, y: 0 });
  const drag = useRef<{ x: number; y: number; ox: number; oy: number } | null>(null);

  const zoom = (factor: number) =>
    setScale((current) => Math.min(MAX_SCALE, Math.max(MIN_SCALE, current * factor)));
  const reset = () => {
    setScale(1);
    setRotation(0);
    setOffset({ x: 0, y: 0 });
  };

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
      else if (event.key === "+" || event.key === "=") zoom(STEP);
      else if (event.key === "-") zoom(1 / STEP);
      else if (event.key === "0") reset();
    };
    const closeOnSwitch = () => onClose();
    window.addEventListener("keydown", onKey);
    window.addEventListener("nuncio:module-changed", closeOnSwitch);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("nuncio:module-changed", closeOnSwitch);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [onClose]);

  return createPortal(
    <div className={styles.backdrop} role="dialog" aria-modal="true" aria-label={name}>
      <div className={styles.bar}>
        <span className={styles.name} title={name}>
          {name}
        </span>
        <button
          type="button"
          className={styles.tool}
          aria-label={t("common.close")}
          title={t("common.close")}
          onClick={onClose}
        >
          <Dismiss20Regular />
        </button>
      </div>

      <div
        className={styles.stage}
        onPointerDown={(event) => {
          drag.current = {
            x: event.clientX,
            y: event.clientY,
            ox: offset.x,
            oy: offset.y,
          };
          event.currentTarget.setPointerCapture(event.pointerId);
        }}
        onPointerMove={(event) => {
          const start = drag.current;
          if (!start) return;
          setOffset({
            x: start.ox + (event.clientX - start.x),
            y: start.oy + (event.clientY - start.y),
          });
        }}
        onPointerUp={() => {
          drag.current = null;
        }}
        onPointerCancel={() => {
          drag.current = null;
        }}
        onDoubleClick={reset}
      >
        <img
          className={styles.image}
          src={url}
          alt={name}
          draggable={false}
          style={{
            transform: `translate(${offset.x}px, ${offset.y}px) rotate(${rotation}deg) scale(${scale})`,
          }}
        />
      </div>

      <div className={styles.tools} role="toolbar">
        <button
          type="button"
          className={styles.tool}
          aria-label={t("todo.viewer.zoomOut")}
          title={t("todo.viewer.zoomOut")}
          onClick={() => zoom(1 / STEP)}
        >
          <Subtract20Regular />
        </button>
        <span className={styles.zoom}>{Math.round(scale * 100)}%</span>
        <button
          type="button"
          className={styles.tool}
          aria-label={t("todo.viewer.zoomIn")}
          title={t("todo.viewer.zoomIn")}
          onClick={() => zoom(STEP)}
        >
          <Add20Regular />
        </button>
        <span className={styles.divider} aria-hidden="true" />
        <button
          type="button"
          className={styles.tool}
          aria-label={t("todo.viewer.rotateLeft")}
          title={t("todo.viewer.rotateLeft")}
          onClick={() => setRotation((current) => current - 90)}
        >
          <ArrowRotateCounterclockwise20Regular />
        </button>
        <button
          type="button"
          className={styles.tool}
          aria-label={t("todo.viewer.rotateRight")}
          title={t("todo.viewer.rotateRight")}
          onClick={() => setRotation((current) => current + 90)}
        >
          <ArrowRotateClockwise20Regular />
        </button>
        <button
          type="button"
          className={styles.tool}
          aria-label={t("todo.viewer.reset")}
          title={t("todo.viewer.reset")}
          onClick={reset}
        >
          <ArrowReset20Regular />
        </button>
      </div>
    </div>,
    document.body,
  );
}
