/**
 * Minimal rich-text editor for signatures (bold / italic / underline / link).
 *
 * Deliberately lightweight: contentEditable + execCommand, plain-text paste,
 * and the sanitizer in lib/richText.ts. No editor framework is loaded into the
 * settings flyout.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import {
  Link24Regular,
  TextBold24Regular,
  TextItalic24Regular,
  TextUnderline24Regular,
} from "@fluentui/react-icons";
import { Button, IconButton, Input, Popover } from "@/components";
import { useT } from "@/lib/i18n";
import {
  escapeHtml,
  isSafeLinkHref,
  normalizeLinkHref,
  sanitizeSignatureHtml,
} from "../lib/richText";
import styles from "../settings.module.css";

export interface RichTextEditorProps {
  value: string;
  onChange: (html: string) => void;
  label?: string;
  placeholder?: string;
  ariaLabel?: string;
  id?: string;
}

interface MarkState {
  bold: boolean;
  italic: boolean;
  underline: boolean;
}

export function RichTextEditor({
  value,
  onChange,
  placeholder,
  ariaLabel,
  id,
}: RichTextEditorProps) {
  const t = useT();
  const editorRef = useRef<HTMLDivElement | null>(null);
  const linkButtonRef = useRef<HTMLButtonElement | null>(null);
  const linkInputRef = useRef<HTMLInputElement | null>(null);
  const [marks, setMarks] = useState<MarkState>({
    bold: false,
    italic: false,
    underline: false,
  });
  const [linkOpen, setLinkOpen] = useState(false);
  const [linkUrl, setLinkUrl] = useState("");
  const [linkError, setLinkError] = useState(false);

  useEffect(() => {
    const editor = editorRef.current;
    if (!editor) return;
    if (document.activeElement === editor) return;
    const safe = sanitizeSignatureHtml(value);
    if (editor.innerHTML !== safe) editor.innerHTML = safe;
  }, [value]);

  useEffect(() => {
    const updateMarks = () => {
      const editor = editorRef.current;
      if (!editor) return;
      const selection = window.getSelection();
      if (!selection || !selection.anchorNode || !editor.contains(selection.anchorNode)) {
        return;
      }
      try {
        setMarks({
          bold: document.queryCommandState("bold"),
          italic: document.queryCommandState("italic"),
          underline: document.queryCommandState("underline"),
        });
      } catch {
        // queryCommandState can throw when the selection is detached.
      }
    };
    document.addEventListener("selectionchange", updateMarks);
    return () => document.removeEventListener("selectionchange", updateMarks);
  }, []);

  const emit = useCallback(() => {
    const editor = editorRef.current;
    if (editor) onChange(editor.innerHTML);
  }, [onChange]);

  /**
   * Sanitize once editing pauses. Running the allowlist rebuild on every
   * keystroke would move the caret, but doing it on blur means the draft the
   * settings page holds (and later persists) is already clean.
   */
  const emitSanitized = useCallback(() => {
    const editor = editorRef.current;
    if (!editor) return;
    const clean = sanitizeSignatureHtml(editor.innerHTML);
    if (clean !== editor.innerHTML) editor.innerHTML = clean;
    onChange(clean);
  }, [onChange]);

  const exec = useCallback(
    (command: string, argument?: string) => {
      const editor = editorRef.current;
      if (!editor) return;
      editor.focus();
      document.execCommand(command, false, argument);
      emit();
    },
    [emit],
  );

  const handlePaste = (event: React.ClipboardEvent<HTMLDivElement>) => {
    event.preventDefault();
    const text = event.clipboardData.getData("text/plain");
    if (text) document.execCommand("insertText", false, text);
    emit();
  };

  /**
   * Dropping HTML into a contentEditable inserts the source markup directly,
   * bypassing the paste handler. Treat drops as plain text as well so a dragged
   * fragment cannot execute in the privileged WebView before the sanitizer
   * runs on blur/save.
   */
  const handleDrop = (event: React.DragEvent<HTMLDivElement>) => {
    event.preventDefault();
    const text = event.dataTransfer.getData("text/plain");
    const editor = editorRef.current;
    if (text && editor) {
      editor.focus();
      document.execCommand("insertText", false, text);
    }
    emit();
  };

  const openLink = () => {
    const editor = editorRef.current;
    if (!editor) return;
    editor.focus();
    const selection = window.getSelection();
    const existing = selection?.anchorNode
      ? findAnchor(selection.anchorNode, editor)
      : null;
    setLinkUrl(existing?.getAttribute("href") ?? "");
    setLinkError(false);
    setLinkOpen(true);
  };

  const applyLink = () => {
    const normalized = normalizeLinkHref(linkUrl);
    if (!isSafeLinkHref(normalized)) {
      setLinkError(true);
      return;
    }
    const editor = editorRef.current;
    if (!editor) return;
    editor.focus();
    const selection = window.getSelection();
    if (!selection || selection.isCollapsed) {
      document.execCommand(
        "insertHTML",
        false,
        `<a href="${escapeHtml(normalized)}">${escapeHtml(normalized)}</a>`,
      );
    } else {
      document.execCommand("createLink", false, normalized);
    }
    setLinkOpen(false);
    setLinkUrl("");
    emit();
  };

  const removeLink = () => {
    exec("unlink");
    setLinkOpen(false);
    setLinkUrl("");
  };

  return (
    <div className={styles.editor}>
      <div className={styles.editorToolbar} role="toolbar" aria-label={t("settings.signatures.formatting")}>
        <IconButton
          icon={<TextBold24Regular />}
          label={t("settings.signatures.bold")}
          size="small"
          selected={marks.bold}
          tooltip={false}
          onClick={() => exec("bold")}
        />
        <IconButton
          icon={<TextItalic24Regular />}
          label={t("settings.signatures.italic")}
          size="small"
          selected={marks.italic}
          tooltip={false}
          onClick={() => exec("italic")}
        />
        <IconButton
          icon={<TextUnderline24Regular />}
          label={t("settings.signatures.underline")}
          size="small"
          selected={marks.underline}
          tooltip={false}
          onClick={() => exec("underline")}
        />
        <span style={{ width: 1, height: 18, background: "var(--border)" }} aria-hidden="true" />
        <IconButton
          ref={linkButtonRef}
          icon={<Link24Regular />}
          label={t("settings.signatures.link")}
          size="small"
          tooltip={false}
          onClick={openLink}
        />
      </div>

      <Popover
        open={linkOpen}
        onClose={() => setLinkOpen(false)}
        anchor={linkButtonRef.current}
        placement="bottom-start"
        minWidth={260}
      >
        <div className={styles.linkForm} style={{ borderBottom: "none" }}>
          <Input
            ref={linkInputRef}
            value={linkUrl}
            onChange={(event) => {
              setLinkUrl(event.target.value);
              setLinkError(false);
            }}
            label={t("settings.signatures.linkUrl")}
            placeholder="https://example.com"
            error={linkError ? t("settings.signatures.linkInvalid") : undefined}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                applyLink();
              }
            }}
            autoFocus
          />
          <Button appearance="primary" size="small" onClick={applyLink}>
            {t("common.apply")}
          </Button>
          <Button appearance="subtle" size="small" onClick={removeLink}>
            {t("settings.signatures.removeLink")}
          </Button>
        </div>
      </Popover>

      <div
        ref={editorRef}
        id={id}
        className={styles.editorSurface}
        contentEditable
        suppressContentEditableWarning
        role="textbox"
        aria-multiline="true"
        aria-label={ariaLabel ?? t("settings.signatures.editorLabel")}
        data-placeholder={placeholder ?? t("settings.signatures.placeholder")}
        onInput={emit}
        onBlur={emitSanitized}
        onPaste={handlePaste}
        onDrop={handleDrop}
      />
    </div>
  );
}

function findAnchor(node: Node, root: HTMLElement): HTMLAnchorElement | null {
  let current: Node | null = node;
  while (current && current !== root) {
    if (current instanceof HTMLAnchorElement) return current;
    current = current.parentNode;
  }
  return null;
}
