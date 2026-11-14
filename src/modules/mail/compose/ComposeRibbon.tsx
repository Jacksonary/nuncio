/**
 * Composer ribbon: three tabs (Format / Insert / Options) modelled on the
 * Windows Mail compose ribbon. Uses the shared Tabs, Select, Menu, Popover,
 * Switch and IconButton primitives.
 */
import { useRef, useState, type ReactNode } from "react";
import type { Editor } from "@tiptap/core";
import {
  ArrowRedo16Regular,
  ArrowUndo16Regular,
  Attach16Regular,
  ClearFormatting16Regular,
  Highlight16Regular,
  Image16Regular,
  LineHorizontal116Regular,
  Link16Regular,
  List16Regular,
  Signature16Regular,
  Table16Regular,
  TextAlignCenter16Regular,
  TextAlignLeft16Regular,
  TextAlignRight16Regular,
  TextBold16Regular,
  TextBulletList16Regular,
  TextColor16Regular,
  TextIndentDecrease16Regular,
  TextIndentIncrease16Regular,
  TextItalic16Regular,
  TextQuote16Regular,
  TextStrikethrough16Regular,
  TextUnderline16Regular,
} from "@fluentui/react-icons";
import {
  Button,
  COLOR_PRESETS,
  IconButton,
  Menu,
  Popover,
  Select,
  Switch,
  Tabs,
  type MenuEntry,
} from "@/components";
import type { Importance, Signature } from "@/ipc/types";
import { useT } from "@/lib/i18n";
import styles from "./ComposeRibbon.module.css";

const FONT_FAMILIES = [
  "Segoe UI",
  "Arial",
  "Calibri",
  "Times New Roman",
  "Georgia",
  "Microsoft YaHei",
  "PingFang SC",
  "Courier New",
];

const FONT_SIZES = [10, 11, 12, 14, 16, 18, 20, 24, 28, 32];

const HIGHLIGHT_COLORS = [
  "#ffff00",
  "#00ff00",
  "#00ffff",
  "#ff00ff",
  "#ffc000",
  "#92d050",
  "#00b0f0",
  "#ff7c80",
  "#d9d9d9",
];

type RibbonTab = "format" | "insert" | "options";

export interface ComposeRibbonProps {
  editor: Editor | null;
  /** Bumped by the host on every editor transaction to refresh active states. */
  revision: number;
  defaultFontFamily: string;
  defaultFontSize: number;
  signatures: Signature[];
  signaturesLoading: boolean;
  importance: Importance;
  requestReadReceipt: boolean;
  spellcheck: boolean;
  onImportanceChange: (value: Importance) => void;
  onRequestReadReceiptChange: (value: boolean) => void;
  onSpellcheckChange: (value: boolean) => void;
  onPickFiles: () => void;
  onPickImage: () => void;
  onOpenLink: () => void;
  onInsertSignature: (signature: Signature) => void;
}

interface SwatchPopoverProps {
  label: string;
  icon: ReactNode;
  colors: string[];
  clearLabel: string;
  onPick: (color: string) => void;
  onClear: () => void;
}

function SwatchPopover({
  label,
  icon,
  colors,
  clearLabel,
  onPick,
  onClear,
}: SwatchPopoverProps) {
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const [open, setOpen] = useState(false);
  return (
    <>
      <IconButton
        ref={triggerRef}
        icon={icon}
        label={label}
        selected={open}
        aria-haspopup="dialog"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
      />
      <Popover
        open={open}
        onClose={() => setOpen(false)}
        anchor={triggerRef.current}
        placement="bottom-start"
        ariaLabel={label}
        minWidth={188}
      >
        <div className={styles.swatches} role="group" aria-label={label}>
          {colors.map((color) => (
            <button
              key={color}
              type="button"
              className={styles.swatch}
              style={{ background: color }}
              aria-label={color}
              title={color}
              onClick={() => {
                onPick(color);
                setOpen(false);
              }}
            />
          ))}
        </div>
        <button
          type="button"
          className={styles.swatchClear}
          onClick={() => {
            onClear();
            setOpen(false);
          }}
        >
          {clearLabel}
        </button>
      </Popover>
    </>
  );
}

interface TablePickerProps {
  onInsert: (rows: number, cols: number) => void;
}

function TablePicker({ onInsert }: TablePickerProps) {
  const t = useT();
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const [open, setOpen] = useState(false);
  const [hover, setHover] = useState({ rows: 3, cols: 3 });

  return (
    <>
      <Button
        ref={triggerRef}
        size="small"
        appearance="transparent"
        icon={<Table16Regular />}
        aria-haspopup="dialog"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
      >
        {t("compose.insert.table")}
      </Button>
      <Popover
        open={open}
        onClose={() => setOpen(false)}
        anchor={triggerRef.current}
        placement="bottom-start"
        ariaLabel={t("compose.insert.tableTitle")}
        minWidth={240}
      >
        <div className={styles.tablePicker}>
          <div className={styles.tablePickerHint}>
            {t("compose.insert.tableSize", { rows: hover.rows, cols: hover.cols })}
          </div>
          <div
            className={styles.tableGrid}
            role="grid"
            aria-label={t("compose.insert.tableHint")}
            onMouseLeave={() => setHover({ rows: 0, cols: 0 })}
          >
            {Array.from({ length: 64 }, (_, index) => {
              const rows = Math.floor(index / 8) + 1;
              const cols = (index % 8) + 1;
              const active = rows <= hover.rows && cols <= hover.cols;
              return (
                <button
                  key={index}
                  type="button"
                  role="gridcell"
                  className={active ? styles.tableCellActive : styles.tableCell}
                  aria-label={t("compose.insert.tableSize", { rows, cols })}
                  onMouseEnter={() => setHover({ rows, cols })}
                  onClick={() => {
                    onInsert(rows, cols);
                    setOpen(false);
                    setHover({ rows: 3, cols: 3 });
                  }}
                />
              );
            })}
          </div>
          <div className={styles.tablePickerFootnote}>
            {t("compose.insert.tableHint")}
          </div>
        </div>
      </Popover>
    </>
  );
}

export function ComposeRibbon({
  editor,
  revision,
  defaultFontFamily,
  defaultFontSize,
  signatures,
  signaturesLoading,
  importance,
  requestReadReceipt,
  spellcheck,
  onImportanceChange,
  onRequestReadReceiptChange,
  onSpellcheckChange,
  onPickFiles,
  onPickImage,
  onOpenLink,
  onInsertSignature,
}: ComposeRibbonProps) {
  const t = useT();
  const [tab, setTab] = useState<RibbonTab>("format");
  const [signatureMenuOpen, setSignatureMenuOpen] = useState(false);
  const signatureRef = useRef<HTMLButtonElement | null>(null);

  // Referencing the revision keeps this component subscribed to editor changes
  // even though the editor instance is stable.
  void revision;

  const chain = () => editor?.chain().focus();
  const can = (command: "undo" | "redo") => {
    if (!editor) return false;
    return command === "undo" ? editor.can().undo() : editor.can().redo();
  };

  const textStyle = (editor?.getAttributes("textStyle") ?? {}) as {
    fontFamily?: string;
    fontSize?: string;
    color?: string;
  };
  const currentFamily = textStyle.fontFamily || defaultFontFamily;
  const parsedSize = Number.parseFloat((textStyle.fontSize ?? "").replace("px", ""));
  const currentSize =
    Number.isFinite(parsedSize) && parsedSize > 0 ? Math.round(parsedSize) : defaultFontSize;
  const sizeOptions = FONT_SIZES.includes(currentSize)
    ? FONT_SIZES
    : [...FONT_SIZES, currentSize].sort((a, b) => a - b);

  const headingValue = editor?.isActive("heading", { level: 1 })
    ? "h1"
    : editor?.isActive("heading", { level: 2 })
      ? "h2"
      : editor?.isActive("heading", { level: 3 })
        ? "h3"
        : "p";

  const bold = editor?.isActive("bold") ?? false;
  const italic = editor?.isActive("italic") ?? false;
  const underline = editor?.isActive("underline") ?? false;
  const strike = editor?.isActive("strike") ?? false;
  const bullets = editor?.isActive("bulletList") ?? false;
  const numbered = editor?.isActive("orderedList") ?? false;
  const quote = editor?.isActive("blockquote") ?? false;
  const alignLeft = editor?.isActive({ textAlign: "left" }) ?? false;
  const alignCenter = editor?.isActive({ textAlign: "center" }) ?? false;
  const alignRight = editor?.isActive({ textAlign: "right" }) ?? false;

  const signatureItems: MenuEntry[] =
    signatures.length > 0
      ? signatures.map((signature) => ({
          id: signature.id,
          label: signature.name,
          icon: <Signature16Regular />,
          onSelect: () => onInsertSignature(signature),
        }))
      : [
          {
            id: "none",
            label: t("compose.signature.none"),
            disabled: true,
          },
        ];

  const formatTab = (
    <div className={styles.row}>
      <div className={styles.group}>
        <Select
          value={currentFamily}
          onChange={(value) => chain()?.setFontFamily(value).run()}
          options={FONT_FAMILIES.map((family) => ({
            value: family,
            label: <span style={{ fontFamily: family }}>{family}</span>,
          }))}
          ariaLabel={t("compose.format.font")}
          size="small"
          className={styles.fontSelect}
        />
        <Select
          value={String(currentSize)}
          onChange={(value) => chain()?.setFontSize(`${value}px`).run()}
          options={sizeOptions.map((size) => ({
            value: String(size),
            label: String(size),
          }))}
          ariaLabel={t("compose.format.size")}
          size="small"
          className={styles.sizeSelect}
        />
      </div>

      <span className={styles.divider} />

      <div className={styles.group}>
        <IconButton
          icon={<TextBold16Regular />}
          label={t("compose.format.bold")}
          selected={bold}
          onClick={() => chain()?.toggleBold().run()}
        />
        <IconButton
          icon={<TextItalic16Regular />}
          label={t("compose.format.italic")}
          selected={italic}
          onClick={() => chain()?.toggleItalic().run()}
        />
        <IconButton
          icon={<TextUnderline16Regular />}
          label={t("compose.format.underline")}
          selected={underline}
          onClick={() => chain()?.toggleUnderline().run()}
        />
        <IconButton
          icon={<TextStrikethrough16Regular />}
          label={t("compose.format.strike")}
          selected={strike}
          onClick={() => chain()?.toggleStrike().run()}
        />
        <SwatchPopover
          label={t("compose.format.color")}
          icon={<TextColor16Regular />}
          colors={COLOR_PRESETS}
          clearLabel={t("compose.format.noColor")}
          onPick={(color) => chain()?.setColor(color).run()}
          onClear={() => chain()?.unsetColor().run()}
        />
        <SwatchPopover
          label={t("compose.format.highlight")}
          icon={<Highlight16Regular />}
          colors={HIGHLIGHT_COLORS}
          clearLabel={t("compose.format.noColor")}
          onPick={(color) => chain()?.setHighlight({ color }).run()}
          onClear={() => chain()?.unsetHighlight().run()}
        />
        <IconButton
          icon={<ClearFormatting16Regular />}
          label={t("compose.format.clear")}
          onClick={() => chain()?.unsetAllMarks().clearNodes().run()}
        />
      </div>

      <span className={styles.divider} />

      <div className={styles.group}>
        <IconButton
          icon={<TextBulletList16Regular />}
          label={t("compose.format.bullets")}
          selected={bullets}
          onClick={() => chain()?.toggleBulletList().run()}
        />
        <IconButton
          icon={<List16Regular />}
          label={t("compose.format.numbering")}
          selected={numbered}
          onClick={() => chain()?.toggleOrderedList().run()}
        />
        <IconButton
          icon={<TextIndentIncrease16Regular />}
          label={t("compose.format.indent")}
          onClick={() => chain()?.increaseIndent().run()}
        />
        <IconButton
          icon={<TextIndentDecrease16Regular />}
          label={t("compose.format.outdent")}
          onClick={() => chain()?.decreaseIndent().run()}
        />
      </div>

      <span className={styles.divider} />

      <div className={styles.group}>
        <Select
          value={headingValue}
          onChange={(value) => {
            const activeChain = chain();
            if (!activeChain) return;
            if (value === "p") {
              activeChain.setParagraph().run();
              return;
            }
            const level = Number(value.slice(1)) as 1 | 2 | 3;
            if (editor?.isActive("heading", { level })) return;
            activeChain.setHeading({ level }).run();
          }}
          options={[
            { value: "p", label: t("compose.format.paragraph") },
            { value: "h1", label: t("compose.format.heading1") },
            { value: "h2", label: t("compose.format.heading2") },
            { value: "h3", label: t("compose.format.heading3") },
          ]}
          ariaLabel={t("compose.format.heading")}
          size="small"
          className={styles.headingSelect}
        />
        <IconButton
          icon={<TextAlignLeft16Regular />}
          label={t("compose.format.alignLeft")}
          selected={alignLeft}
          onClick={() => chain()?.setTextAlign("left").run()}
        />
        <IconButton
          icon={<TextAlignCenter16Regular />}
          label={t("compose.format.alignCenter")}
          selected={alignCenter}
          onClick={() => chain()?.setTextAlign("center").run()}
        />
        <IconButton
          icon={<TextAlignRight16Regular />}
          label={t("compose.format.alignRight")}
          selected={alignRight}
          onClick={() => chain()?.setTextAlign("right").run()}
        />
        <IconButton
          icon={<TextQuote16Regular />}
          label={t("compose.format.quote")}
          selected={quote}
          onClick={() => chain()?.toggleBlockquote().run()}
        />
      </div>

      <span className={styles.divider} />

      <div className={styles.group}>
        <IconButton
          icon={<ArrowUndo16Regular />}
          label={t("compose.format.undo")}
          disabled={!can("undo")}
          onClick={() => chain()?.undo().run()}
        />
        <IconButton
          icon={<ArrowRedo16Regular />}
          label={t("compose.format.redo")}
          disabled={!can("redo")}
          onClick={() => chain()?.redo().run()}
        />
      </div>
    </div>
  );

  const insertTab = (
    <div className={styles.row}>
      <div className={styles.group}>
        <Button
          size="small"
          appearance="transparent"
          icon={<Attach16Regular />}
          onClick={onPickFiles}
        >
          {t("compose.insert.file")}
        </Button>
        <Button
          size="small"
          appearance="transparent"
          icon={<Image16Regular />}
          onClick={onPickImage}
        >
          {t("compose.insert.image")}
        </Button>
        <Button
          size="small"
          appearance="transparent"
          icon={<Link16Regular />}
          onClick={onOpenLink}
        >
          {t("compose.insert.link")}
        </Button>
      </div>
      <span className={styles.divider} />
      <div className={styles.group}>
        <TablePicker
          onInsert={(rows, cols) =>
            chain()?.insertTable({ rows, cols, withHeaderRow: true }).run()
          }
        />
        <Button
          size="small"
          appearance="transparent"
          icon={<LineHorizontal116Regular />}
          onClick={() => chain()?.setHorizontalRule().run()}
        >
          {t("compose.insert.horizontalRule")}
        </Button>
      </div>
      <span className={styles.divider} />
      <div className={styles.group}>
        <Button
          ref={signatureRef}
          size="small"
          appearance="transparent"
          icon={<Signature16Regular />}
          aria-haspopup="menu"
          aria-expanded={signatureMenuOpen}
          disabled={signaturesLoading}
          onClick={() => setSignatureMenuOpen((value) => !value)}
        >
          {t("compose.insert.signature")}
        </Button>
        <Menu
          open={signatureMenuOpen}
          onClose={() => setSignatureMenuOpen(false)}
          items={signatureItems}
          anchor={signatureRef.current}
          placement="bottom-start"
          ariaLabel={t("compose.insert.signature")}
        />
      </div>
    </div>
  );

  const optionsTab = (
    <div className={styles.row}>
      <div className={styles.group}>
        <span className={styles.groupLabel}>{t("compose.options.importance")}</span>
        <Select
          value={importance}
          onChange={(value) => onImportanceChange(value as Importance)}
          options={[
            { value: "high", label: t("compose.options.importanceHigh") },
            { value: "normal", label: t("compose.options.importanceNormal") },
            { value: "low", label: t("compose.options.importanceLow") },
          ]}
          ariaLabel={t("compose.options.importance")}
          size="small"
          className={styles.importanceSelect}
        />
      </div>
      <span className={styles.divider} />
      <div className={styles.group}>
        <Switch
          checked={requestReadReceipt}
          onChange={onRequestReadReceiptChange}
          label={t("compose.options.readReceipt")}
        />
        <Switch
          checked={spellcheck}
          onChange={onSpellcheckChange}
          label={t("compose.options.spellcheck")}
        />
      </div>
    </div>
  );

  return (
    <div className={styles.ribbon}>
      <Tabs
        items={[
          { id: "format", label: t("compose.tab.format") },
          { id: "insert", label: t("compose.tab.insert") },
          { id: "options", label: t("compose.tab.options") },
        ]}
        value={tab}
        onChange={(id) => setTab(id as RibbonTab)}
        appearance="underline"
        ariaLabel={t("compose.toolbarLabel")}
        className={styles.tabs}
      />
      <div className={styles.body} role="toolbar" aria-label={t("compose.toolbarLabel")}>
        {tab === "format" ? formatTab : tab === "insert" ? insertTab : optionsTab}
      </div>
    </div>
  );
}
