import { useEffect, useState } from "react";
import { cn } from "@/lib/cn";
import { getInitials } from "@/lib/format";
import styles from "./Avatar.module.css";

export interface AvatarProps {
  name?: string | null;
  email?: string | null;
  /** data: URL or local image source. */
  src?: string | null;
  size?: number;
  square?: boolean;
  className?: string;
  /** Decorative by default; pass a name for an accessible image label. */
  ariaLabel?: string;
}

const PALETTE = [
  "#0f6cbd",
  "#2b88d8",
  "#038387",
  "#107c10",
  "#498205",
  "#8764b8",
  "#5c2e91",
  "#c239b3",
  "#c50f1f",
  "#d13438",
  "#ca5010",
  "#986f0b",
  "#4f6bed",
  "#8e562e",
];

function hashString(value: string): number {
  let hash = 0;
  for (let index = 0; index < value.length; index += 1) {
    hash = (hash * 31 + value.charCodeAt(index)) | 0;
  }
  return Math.abs(hash);
}

/** Deterministic color so the same person always gets the same avatar tint. */
export function avatarColor(name?: string | null, email?: string | null): string {
  const key = (email || name || "").trim().toLowerCase();
  if (!key) return PALETTE[0];
  return PALETTE[hashString(key) % PALETTE.length];
}

export function Avatar({
  name,
  email,
  src,
  size = 32,
  square = false,
  className,
  ariaLabel,
}: AvatarProps) {
  const [imageFailed, setImageFailed] = useState(false);
  useEffect(() => setImageFailed(false), [src]);
  const initials = getInitials(name, email);
  const showImage = Boolean(src) && !imageFailed;
  return (
    <span
      className={cn(styles.avatar, square && styles.square, !showImage && styles.ring, className)}
      style={{
        width: size,
        height: size,
        fontSize: Math.max(10, Math.round(size * 0.36)),
        background: showImage ? undefined : avatarColor(name, email),
      }}
      role={ariaLabel ? "img" : undefined}
      aria-label={ariaLabel}
      aria-hidden={ariaLabel ? undefined : true}
      title={ariaLabel}
    >
      {showImage ? (
        <img
          className={styles.image}
          src={src ?? undefined}
          alt=""
          draggable={false}
          onError={() => setImageFailed(true)}
        />
      ) : (
        initials
      )}
    </span>
  );
}
