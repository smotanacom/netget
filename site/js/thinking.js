// What a model is writing, split into the thinking the LLM panel shows as "Thinking…" and
// the answer NetGet parses.
//
// Only a model that reasons natively has thinking to show: Qwen3 writes `<think>…</think>`
// and then its answer. Some chat templates open the block in the prompt, so the text may start
// mid-thought with only the closing `</think>` in it. Every other model's text is all answer:
// nothing is split out of it, whatever it contains.
//
// Pure (no DOM): web/test/smoke.mjs imports it and checks the split under Node.

const OPEN = '<think>';
const CLOSE = '</think>';

// How much of `text`'s end is the start of `tag` (`<`, `</thi`, …), which the next chunk may
// complete.
function partialTagAt(text, tag) {
    for (let n = Math.min(tag.length - 1, text.length); n > 0; n -= 1) {
        if (text.endsWith(tag.slice(0, n))) return n;
    }
    return 0;
}

/**
 * Split what a model has written so far.
 *
 * `thinks`: the model reasons natively; without it the whole text is the answer. `final`: the
 * text is complete, so nothing is pending any more.
 *
 * Returns `{thinking, answer, thinkingDone}`:
 * - `thinking`: the think block's text, without the tags;
 * - `answer`: what NetGet gets, and what the panel shows as the answer — the text after the
 *   think block;
 * - `thinkingDone`: the think block is closed and the answer has begun (or the text is final).
 */
export function splitThinking(text, { thinks = false, final = false } = {}) {
    const s = String(text ?? '');
    if (!thinks) return { thinking: '', answer: s.replace(/^\s+/, ''), thinkingDone: true };
    const lead = s.replace(/^\s+/, '');
    let thinking = '';
    let answer = s;
    let inThought = false;
    const close = s.indexOf(CLOSE);
    if (close >= 0) {
        const open = s.indexOf(OPEN);
        thinking = s.slice(open >= 0 && open < close ? open + OPEN.length : 0, close);
        answer = s.slice(close + CLOSE.length);
    } else if (lead.startsWith(OPEN)) {
        // Unclosed: still thinking, or (final) it ran out of tokens before answering.
        thinking = lead.slice(OPEN.length);
        answer = '';
        inThought = !final;
    } else if (!final && lead && OPEN.startsWith(lead)) {
        // `<thi`: a think block is opening; nothing to show yet.
        answer = '';
        inThought = true;
    } else if (!final && lead && !/^[{[`]/.test(lead)) {
        // The template opened the block in the prompt: text before `</think>` is thinking.
        // (Text that starts like JSON is an answer written with thinking switched off.)
        thinking = s;
        answer = '';
        inThought = true;
    }
    if (inThought) {
        const cut = partialTagAt(thinking, CLOSE);
        if (cut) thinking = thinking.slice(0, thinking.length - cut);
    }
    answer = answer.replace(/^\s+/, '');
    return {
        thinking: thinking.trim(),
        answer,
        thinkingDone: final || (!inThought && answer.length > 0),
    };
}
