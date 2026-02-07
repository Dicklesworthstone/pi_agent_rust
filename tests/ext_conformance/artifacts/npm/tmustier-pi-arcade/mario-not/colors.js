// @ts-check
"use strict";

// ANSI color codes for terminal output
const COLORS = {
	reset: "\x1b[0m",
	red: "\x1b[31m",
	green: "\x1b[32m",
	yellow: "\x1b[33m",
	blue: "\x1b[34m",
	magenta: "\x1b[35m",
	cyan: "\x1b[36m",
	white: "\x1b[37m",
	gray: "\x1b[90m",
	brightRed: "\x1b[91m",
	brightGreen: "\x1b[92m",
	brightYellow: "\x1b[93m",
	brightCyan: "\x1b[96m",
	orange: "\x1b[38;5;208m",
	brown: "\x1b[38;5;94m",
};

/**
 * Wrap a string with color codes
 * @param {string} text
 * @param {string} color
 * @returns {string}
 */
function colorize(text, color) {
	return `${color}${text}${COLORS.reset}`;
}

module.exports = {
	COLORS,
	colorize,
};
