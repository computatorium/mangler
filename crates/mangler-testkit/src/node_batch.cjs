'use strict';
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const { setImmediate: immediate } = require('node:timers/promises');
const dir = process.argv[2];
const observerSource = fs.readFileSync(path.join(dir, 'observer.js'), 'utf8');
const files = JSON.parse(fs.readFileSync(path.join(dir, 'manifest.json'), 'utf8'));
(async () => {
    const reports = [];
    for (const file of files) {
        const context = vm.createContext({ setInterval, clearInterval, setTimeout, clearTimeout, atob, btoa, TextEncoder, TextDecoder, URL, URLSearchParams, Buffer });
        const observer = vm.runInContext(observerSource, context, { timeout: 2000 });
        const rejections = new Map();
        const rejected = (reason, promise) => rejections.set(promise, reason);
        const handled = promise => rejections.delete(promise);
        process.on('unhandledRejection', rejected);
        process.on('rejectionHandled', handled);
        let outcome;
        try {
            // Evaluate the exact on-disk artifact; do not strip guards or rewrite
            // function bodies to make the differential pass.
            vm.runInContext(fs.readFileSync(path.join(dir, file), 'utf8'), context,
                { filename: file, timeout: 2000 });
        } catch (error) {
            if (error && error.code === 'ERR_SCRIPT_EXECUTION_TIMEOUT') throw error;
            outcome = ['throw', observer.thrown(error)];
        }
        await immediate();
        await immediate();
        if (!outcome) outcome = ['value', observer.value(vm.runInContext('globalThis.__out', context, { timeout: 2000 }))];
        reports.push({ outcome, trace: observer.trace(), rejections: [...rejections.values()].map(e => observer.thrown(e)) });
        process.removeListener('unhandledRejection', rejected);
        process.removeListener('rejectionHandled', handled);
    }
    process.stdout.write(JSON.stringify(reports));
})().catch(error => { process.stderr.write(String(error.stack || error)); process.exitCode = 1; });
