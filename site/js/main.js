// Copy buttons
document.querySelectorAll('.copy-btn').forEach(function (btn) {
    btn.addEventListener('click', function () {
        var code = document.getElementById(btn.getAttribute('data-target'));
        if (!code) return;
        navigator.clipboard.writeText(code.textContent).then(function () {
            btn.textContent = 'Copied!';
            btn.classList.add('copied');
            setTimeout(function () {
                btn.textContent = 'Copy';
                btn.classList.remove('copied');
            }, 2000);
        });
    });
});

// Light / dark theme toggle
(function () {
    var root = document.documentElement;
    var btn = document.getElementById('theme-toggle');
    if (!btn) return;

    function current() {
        var t = root.getAttribute('data-theme');
        if (t) return t;
        return window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
    }

    function render() {
        btn.textContent = current() === 'light' ? '☾' : '☀';
    }

    btn.addEventListener('click', function () {
        var next = current() === 'light' ? 'dark' : 'light';
        root.setAttribute('data-theme', next);
        try { localStorage.setItem('theme', next); } catch (e) {}
        render();
    });

    render();
})();

// Hero terminal: cycle through example scenarios with a typing effect
(function () {
    var term = document.getElementById('hero-term');
    if (!term) return;
    if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) return;

    var scenarios = [
        {
            prompt: 'http server on 8080, mock a payments API',
            lines: [
                ['tag-info', 'INFO', 'HTTP listening on :8080'],
                ['tag-req', 'REQ', ' POST /charge {"amount": 4200}'],
                ['tag-llm', 'LLM', ' 200 {"status": "paid", "id": "ch_8f3k2"}']
            ]
        },
        {
            prompt: 'ssh server on 2222, act like Ubuntu, log every command',
            lines: [
                ['tag-info', 'INFO', 'SSH listening on :2222'],
                ['tag-req', 'REQ', ' login root:hunter2, accepted, logged'],
                ['tag-llm', 'LLM', ' "Welcome to Ubuntu 22.04.3 LTS"']
            ]
        },
        {
            prompt: 'mysql server, invent realistic data for any query',
            lines: [
                ['tag-info', 'INFO', 'MySQL listening on :3306'],
                ['tag-req', 'REQ', ' SELECT name, email FROM users LIMIT 2'],
                ['tag-llm', 'LLM', ' 2 rows · alice@corp.io · bob@corp.io']
            ]
        },
        {
            prompt: 'dns server on 5353, resolve *.local to 127.0.0.1',
            lines: [
                ['tag-info', 'INFO', 'DNS listening on :5353'],
                ['tag-req', 'REQ', ' A? app.local'],
                ['tag-llm', 'LLM', ' app.local → 127.0.0.1']
            ]
        }
    ];

    var i = 0;

    function esc(s) {
        return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
    }

    function play(scenario, done) {
        term.innerHTML =
            '<div class="line"><span class="prompt">&gt;</span> <span class="typed"></span><span class="caret"></span></div>';
        var typed = term.querySelector('.typed');
        var chars = scenario.prompt.split('');
        var pos = 0;

        function typeChar() {
            if (pos < chars.length) {
                typed.textContent += chars[pos++];
                setTimeout(typeChar, 28);
            } else {
                setTimeout(function () { showLine(0); }, 450);
            }
        }

        function showLine(n) {
            if (n >= scenario.lines.length) { done(); return; }
            var l = scenario.lines[n];
            var div = document.createElement('div');
            div.className = 'line out';
            div.innerHTML = '<span class="' + l[0] + '">' + l[1] + '</span> ' + esc(l[2]);
            term.appendChild(div);
            setTimeout(function () { showLine(n + 1); }, 550);
        }

        typeChar();
    }

    function next() {
        play(scenarios[i], function () {
            i = (i + 1) % scenarios.length;
            setTimeout(next, 3200);
        });
    }

    setTimeout(next, 1800);
})();
