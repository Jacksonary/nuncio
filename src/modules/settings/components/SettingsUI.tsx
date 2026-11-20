/** Small layout primitives shared by every settings page. */
import { useState, type ReactNode } from "react";
import { ArrowLeft24Regular, ChevronRight24Regular, Info24Regular, Warning24Regular, ErrorCircle24Regular, CheckmarkCircle24Regular } from "@fluentui/react-icons";
import { IconButton } from "@/components";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import styles from "../settings.module.css";

export interface SettingsPageProps {
  title: ReactNode;
  subtitle?: ReactNode;
  onBack?: () => void;
  actions?: ReactNode;
  children: ReactNode;
}

/** Page chrome with a sticky header and back navigation (Win Mail style). */
export function SettingsPage({ title, subtitle, onBack, actions, children }: SettingsPageProps) {
  const t = useT();
  return (
    <div className={styles.page}>
      <div className={styles.pageHeader}>
        {onBack ? (
          <span className={styles.backButton}>
            <IconButton
              icon={<ArrowLeft24Regular />}
              label={t("common.back")}
              appearance="subtle"
              size="small"
              onClick={onBack}
            />
          </span>
        ) : null}
        <div className={styles.pageHeading}>
          <div className={styles.pageTitle}>{title}</div>
          {subtitle ? <div className={styles.pageSubtitle}>{subtitle}</div> : null}
        </div>
        {actions ? <div className={styles.listActions}>{actions}</div> : null}
      </div>
      <div className={styles.pageBody}>{children}</div>
    </div>
  );
}

export interface SectionProps {
  title?: ReactNode;
  hint?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
  className?: string;
}

export function Section({ title, hint, actions, children, className }: SectionProps) {
  return (
    <section className={cn(styles.section, className)}>
      {title || actions ? (
        <div className={styles.sectionHeader}>
          {title ? <h2 className={styles.sectionTitle}>{title}</h2> : <span />}
          {actions ? <div className={styles.listActions}>{actions}</div> : hint}
        </div>
      ) : null}
      {children}
    </section>
  );
}

/**
 * A named group that hosts several sections. Merged pages (Mail, Calendar &
 * contacts & tasks) put one group per former page, so the module keeps its own
 * heading while its settings stay plain `Section`s underneath.
 */
export interface SectionGroupProps {
  title: ReactNode;
  hint?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
}

export function SectionGroup({ title, hint, actions, children }: SectionGroupProps) {
  return (
    <section className={styles.group}>
      <div className={styles.groupHeader}>
        <div className={styles.groupHeading}>
          <h2 className={styles.groupTitle}>{title}</h2>
          {hint ? <div className={styles.groupHint}>{hint}</div> : null}
        </div>
        {actions ? <div className={styles.listActions}>{actions}</div> : null}
      </div>
      <div className={styles.groupBody}>{children}</div>
    </section>
  );
}

/** A collapsed block that reveals its content on demand. */
export function Disclosure({
  label,
  defaultOpen = false,
  children,
}: {
  label: ReactNode;
  defaultOpen?: boolean;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <div className={styles.disclosure}>
      <button
        type="button"
        className={styles.disclosureToggle}
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
      >
        <span
          className={cn(styles.disclosureChevron, open && styles.disclosureChevronOpen)}
          aria-hidden="true"
        >
          <ChevronRight24Regular />
        </span>
        <span>{label}</span>
      </button>
      {open ? <div className={styles.disclosureBody}>{children}</div> : null}
    </div>
  );
}

/** A card row that navigates to a sub-page instead of hosting a control. */
export function LinkRow({
  label,
  description,
  icon,
  onClick,
}: {
  label: ReactNode;
  description?: ReactNode;
  icon?: ReactNode;
  onClick: () => void;
}) {
  return (
    <button type="button" className={styles.navRow} onClick={onClick}>
      {icon ? (
        <span className={styles.homeIcon} aria-hidden="true">
          {icon}
        </span>
      ) : null}
      <span className={styles.navRowText}>
        <span className={styles.rowLabel}>{label}</span>
        {description ? <span className={styles.rowDesc}>{description}</span> : null}
      </span>
      <span className={styles.navRowChevron} aria-hidden="true">
        <ChevronRight24Regular />
      </span>
    </button>
  );
}

export function Card({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cn(styles.card, className)}>{children}</div>;
}

export interface SettingRowProps {
  label: ReactNode;
  description?: ReactNode;
  children?: ReactNode;
  /** Let the control grow (text fields) instead of staying compact. */
  wide?: boolean;
  className?: string;
  htmlFor?: string;
}

export function SettingRow({
  label,
  description,
  children,
  wide = false,
  className,
  htmlFor,
}: SettingRowProps) {
  return (
    <div className={cn(styles.row, className)}>
      <div className={styles.rowText}>
        {htmlFor ? (
          <label className={styles.rowLabel} htmlFor={htmlFor}>
            {label}
          </label>
        ) : (
          <span className={styles.rowLabel}>{label}</span>
        )}
        {description ? <div className={styles.rowDesc}>{description}</div> : null}
      </div>
      {children ? (
        <div className={cn(styles.rowControl, wide && styles.rowControlWide)}>{children}</div>
      ) : null}
    </div>
  );
}

/** A full-width block inside a card (fields, editors, lists). */
export function StackRow({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cn(styles.stackRow, className)}>{children}</div>;
}

export type BannerTone = "info" | "warning" | "error" | "success";

const BANNER_ICONS: Record<BannerTone, ReactNode> = {
  info: <Info24Regular />,
  warning: <Warning24Regular />,
  error: <ErrorCircle24Regular />,
  success: <CheckmarkCircle24Regular />,
};

export interface BannerProps {
  tone?: BannerTone;
  title?: ReactNode;
  children?: ReactNode;
  actions?: ReactNode;
}

export function Banner({ tone = "info", title, children, actions }: BannerProps) {
  const toneClass =
    tone === "warning"
      ? styles.warnBanner
      : tone === "error"
        ? styles.errorBanner
        : tone === "success"
          ? styles.successBanner
          : styles.infoBanner;
  return (
    <div className={cn(styles.banner, toneClass)} role={tone === "error" ? "alert" : "status"}>
      <span className={styles.bannerIcon} aria-hidden="true">
        {BANNER_ICONS[tone]}
      </span>
      <div className={styles.bannerBody}>
        {title ? <div className={styles.bannerTitle}>{title}</div> : null}
        {children ? <div>{children}</div> : null}
        {actions ? <div className={styles.actions}>{actions}</div> : null}
      </div>
    </div>
  );
}

export function Hint({ children }: { children: ReactNode }) {
  return <p className={styles.hint}>{children}</p>;
}
