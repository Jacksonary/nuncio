/**
 * Tiptap extension set for the composer.
 *
 * StarterKit v3 already ships bold/italic/underline/strike/lists/quote/undo,
 * so only the extras are added here: links, images (with a cid mapping
 * attribute), text style/color/font size, highlight, alignment, tables,
 * placeholder, paragraph indent and the signature wrapper (`div.nuncio-signature`).
 */
import { Extension, Node, mergeAttributes, type CommandProps } from "@tiptap/core";
import { Plugin } from "@tiptap/pm/state";
import StarterKit from "@tiptap/starter-kit";
import Link from "@tiptap/extension-link";
import Image from "@tiptap/extension-image";
import Highlight from "@tiptap/extension-highlight";
import TextAlign from "@tiptap/extension-text-align";
import Placeholder from "@tiptap/extension-placeholder";
import { TextStyleKit } from "@tiptap/extension-text-style";
import { Color } from "@tiptap/extension-color";
import { TableKit } from "@tiptap/extension-table";
import {
  SIGNATURE_CLASS,
  SIGNATURE_ID_ATTRIBUTE,
  SIGNATURE_NODE_NAME,
} from "./signature";

declare module "@tiptap/core" {
  interface Commands<ReturnType> {
    indent: {
      /** Increase the left indent of the current paragraph/heading. */
      increaseIndent: () => ReturnType;
      /** Decrease the left indent of the current paragraph/heading. */
      decreaseIndent: () => ReturnType;
    };
  }
}

interface IndentOptions {
  types: string[];
  maxLevel: number;
}

function changeIndent(
  { state, commands }: CommandProps,
  delta: number,
  options: IndentOptions,
): boolean {
  const { $from } = state.selection;
  const node = $from.parent;
  if (!options.types.includes(node.type.name)) return false;
  const current = Number(node.attrs.indent ?? 0) || 0;
  const next = Math.max(0, Math.min(options.maxLevel, current + delta));
  if (next === current) return false;
  return commands.updateAttributes(node.type, { indent: next });
}

/** Adds a numeric `indent` attribute rendered as a left margin. */
export const Indent = Extension.create<IndentOptions>({
  name: "indent",

  addOptions() {
    return { types: ["paragraph", "heading"], maxLevel: 8 };
  },

  addGlobalAttributes() {
    return [
      {
        types: this.options.types,
        attributes: {
          indent: {
            default: 0,
            parseHTML: (element) => {
              const raw = element.getAttribute("data-indent");
              const value = raw ? Number.parseInt(raw, 10) : 0;
              if (!Number.isFinite(value)) return 0;
              return Math.max(0, Math.min(this.options.maxLevel, value));
            },
            renderHTML: (attributes) => {
              const level = Number(attributes.indent) || 0;
              if (level <= 0) return {};
              return {
                "data-indent": String(level),
                style: `margin-left:${level * 24}px`,
              };
            },
          },
        },
      },
    ];
  },

  addCommands() {
    const options = this.options;
    return {
      increaseIndent:
        () =>
        (props: CommandProps) =>
          changeIndent(props, 1, options),
      decreaseIndent:
        () =>
        (props: CommandProps) =>
          changeIndent(props, -1, options),
    };
  },
});

/**
 * Image node with a persistent `data-content-id`. Inline pictures are displayed
 * from their base64 source while composing and serialized back to
 * `src="cid:<contentId>"` when the draft is saved or sent.
 */
export const ComposeImage = Image.extend({
  addAttributes() {
    return {
      ...this.parent?.(),
      contentId: {
        default: null,
        parseHTML: (element: HTMLElement) => element.getAttribute("data-content-id"),
        renderHTML: (attributes: Record<string, unknown>) =>
          attributes.contentId
            ? { "data-content-id": String(attributes.contentId) }
            : {},
      },
    };
  },
});

/**
 * The signature wrapper shared with the backend: a top-level
 * `<div class="nuncio-signature">…</div>` block. Without this node the editor
 * would flatten the div into plain paragraphs, losing the wrapper — and with
 * it the ability to find and replace the signature when the From identity
 * changes.
 *
 * The optional `data-nuncio-signature-id` attribute remembers which signature
 * was inserted; `finalizeHtml` strips `data-*` attributes before the draft is
 * saved or sent, so recipients only see the class.
 */
export const SignatureBlock = Node.create({
  name: SIGNATURE_NODE_NAME,
  group: "block",
  content: "block*",
  defining: true,
  isolating: true,
  selectable: true,

  addAttributes() {
    return {
      signatureId: {
        default: null,
        parseHTML: (element: HTMLElement) =>
          element.getAttribute(SIGNATURE_ID_ATTRIBUTE) ??
          element.getAttribute("data-nuncio-signature"),
        renderHTML: (attributes: Record<string, unknown>) => {
          const id = attributes.signatureId;
          return typeof id === "string" && id
            ? { [SIGNATURE_ID_ATTRIBUTE]: id }
            : {};
        },
      },
    };
  },

  parseHTML() {
    return [
      { tag: `div.${SIGNATURE_CLASS}` },
      { tag: "div[data-nuncio-signature]" },
      { tag: `div[${SIGNATURE_ID_ATTRIBUTE}]` },
    ];
  },

  renderHTML({ HTMLAttributes }) {
    return [
      "div",
      mergeAttributes({ class: SIGNATURE_CLASS }, HTMLAttributes),
      0,
    ];
  },
});

export interface ComposeExtensionsOptions {
  placeholder: string;
  /** Evaluated per transaction; true blocks every document change. */
  isLocked: () => boolean;
}

interface ComposeLockOptions {
  /**
   * Returns true while the composer must reject document changes.
   *
   * Enforced in `filterTransaction`, i.e. at the editor's own gate, rather than
   * in each command: the ribbon keeps a reference to the editor for the whole
   * render, so a click on a stale button can still call `toggleBold()` after the
   * composer was frozen. `setEditable(false)` only stops input events, not
   * programmatic commands, and there are two dozen of those.
   */
  isLocked: () => boolean;
}

/**
 * Reject every document-changing transaction while the composer is frozen.
 *
 * Selection-only transactions pass so the caret does not get stuck and so
 * `setEditable` can still take effect.
 */
const ComposeLock = Extension.create<ComposeLockOptions>({
  name: "composeLock",
  addOptions() {
    return { isLocked: () => false };
  },
  addProseMirrorPlugins() {
    const { isLocked } = this.options;
    return [
      new Plugin({
        filterTransaction: (transaction) =>
          transaction.docChanged ? !isLocked() : true,
      }),
    ];
  },
});

export function createComposeExtensions({
  placeholder,
  isLocked,
}: ComposeExtensionsOptions) {
  return [
    ComposeLock.configure({ isLocked }),
    StarterKit.configure({
      heading: { levels: [1, 2, 3] },
      link: false,
    }),
    Link.configure({
      openOnClick: false,
      autolink: true,
      linkOnPaste: true,
      HTMLAttributes: {
        rel: "noopener noreferrer",
        target: "_blank",
      },
    }),
    ComposeImage.configure({
      inline: false,
      allowBase64: true,
    }),
    TextStyleKit.configure({ color: false }),
    Color,
    Highlight.configure({ multicolor: true }),
    TextAlign.configure({ types: ["heading", "paragraph"] }),
    TableKit.configure({ table: { resizable: true } }),
    Placeholder.configure({ placeholder }),
    Indent,
    SignatureBlock,
  ];
}
