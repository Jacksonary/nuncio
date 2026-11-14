/**
 * Contact photo handling.
 *
 * The IPC contract has no file-read command, so the editor accepts photos via
 * drag & drop or clipboard paste. Images are read with `FileReader`, downscaled
 * with a canvas and stored as a small `data:` URL (<= 256 px), which is exactly
 * what `Contact.photo` is specified to hold.
 */
export const PHOTO_MAX_SIZE = 256;
/** Refuse absurdly large source images before decoding them. */
export const PHOTO_MAX_FILE_BYTES = 20 * 1024 * 1024;
const PHOTO_QUALITY = 0.82;

function readAsDataUrl(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const result = typeof reader.result === "string" ? reader.result : "";
      if (result.startsWith("data:image/")) resolve(result);
      else reject(new Error("unsupported-image"));
    };
    reader.onerror = () => reject(reader.error ?? new Error("read-failed"));
    reader.readAsDataURL(blob);
  });
}

function loadImage(source: string): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const image = new Image();
    image.onload = () => resolve(image);
    image.onerror = () => reject(new Error("decode-failed"));
    image.src = source;
  });
}

/** Downscale any image data URL to <= max px and re-encode as JPEG. */
export async function downscaleImage(
  dataUrl: string,
  max = PHOTO_MAX_SIZE,
): Promise<string> {
  const image = await loadImage(dataUrl);
  const width = image.naturalWidth || image.width;
  const height = image.naturalHeight || image.height;
  if (!width || !height) throw new Error("empty-image");
  const scale = Math.min(1, max / Math.max(width, height));
  const targetWidth = Math.max(1, Math.round(width * scale));
  const targetHeight = Math.max(1, Math.round(height * scale));
  const canvas = document.createElement("canvas");
  canvas.width = targetWidth;
  canvas.height = targetHeight;
  const context = canvas.getContext("2d");
  if (!context) throw new Error("canvas-unavailable");
  // JPEG has no alpha channel: transparency becomes white, like Outlook.
  context.fillStyle = "#ffffff";
  context.fillRect(0, 0, targetWidth, targetHeight);
  context.imageSmoothingEnabled = true;
  context.imageSmoothingQuality = "high";
  context.drawImage(image, 0, 0, targetWidth, targetHeight);
  return canvas.toDataURL("image/jpeg", PHOTO_QUALITY);
}

/** True when a dropped/pasted file can become a contact photo. */
export function isImageFile(file: File): boolean {
  return file.type.startsWith("image/");
}

/** Read a picked/dropped/pasted image file into a downscaled data URL. */
export async function photoFromFile(file: File): Promise<string> {
  if (!isImageFile(file)) throw new Error("unsupported-image");
  if (file.size > PHOTO_MAX_FILE_BYTES) throw new Error("too-large");
  const raw = await readAsDataUrl(file);
  return downscaleImage(raw);
}

/** Extract the first image from clipboard data, or null. */
export function imageFileFromClipboard(data: DataTransfer): File | null {
  const items = Array.from(data.items ?? []);
  for (const item of items) {
    if (item.kind === "file" && item.type.startsWith("image/")) {
      const file = item.getAsFile();
      if (file) return file;
    }
  }
  const files = Array.from(data.files ?? []);
  return files.find(isImageFile) ?? null;
}
