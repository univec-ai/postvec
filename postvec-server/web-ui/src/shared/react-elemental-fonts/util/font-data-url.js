/**
 * Generate a CSS data URI for a base64-encoded font file.
 *
 * @param {string} base64 Base64-encoded representation of the font file.
 */
const fontDataURL = (base64) => {
  return `url(data:application/x-font-ttf;base64,${base64})`;
};

export default fontDataURL;