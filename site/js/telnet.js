// Stateful line-mode Telnet decoding. TCP chunks have no relationship to command
// boundaries or UTF-8 characters, so both parsers live for the whole connection.
const IAC = 255, DONT = 254, DO = 253, WONT = 252, WILL = 251, SB = 250, SE = 240;

export class TelnetDecoder {
    constructor() {
        this.state = 'data';
        this.optionCommand = null;
        this.decoder = new TextDecoder();
        this.previousCR = false;
    }

    push(bytes) {
        const data = [];
        const replies = [];
        for (const byte of bytes) {
            switch (this.state) {
            case 'data':
                if (byte === IAC) this.state = 'command';
                else data.push(byte);
                break;
            case 'command':
                if (byte === SB) this.state = 'subnegotiation';
                else if ([DO, DONT, WILL, WONT].includes(byte)) {
                    this.optionCommand = byte;
                    this.state = 'option';
                } else {
                    if (byte === IAC) data.push(IAC);
                    this.state = 'data';
                }
                break;
            case 'option':
                if (this.optionCommand === WILL) replies.push(IAC, DONT, byte);
                else if (this.optionCommand === DO) replies.push(IAC, WONT, byte);
                this.state = 'data';
                break;
            case 'subnegotiation':
                if (byte === IAC) this.state = 'subnegotiation-command';
                break;
            case 'subnegotiation-command':
                // IAC IAC is escaped data inside the subnegotiation, not its end.
                this.state = byte === SE ? 'data' : 'subnegotiation';
                break;
            }
        }
        const text = this.normalize(this.decoder.decode(new Uint8Array(data), { stream: true }));
        return { text, replies: new Uint8Array(replies) };
    }

    finish() {
        return this.normalize(this.decoder.decode());
    }

    normalize(text) {
        let result = '';
        for (const char of text) {
            if (char === '\n' && !this.previousCR) result += '\r';
            result += char;
            this.previousCR = char === '\r';
        }
        return result;
    }
}
