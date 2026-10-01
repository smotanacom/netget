// The remaining browser hyper servers, driven by Node's independent HTTP parser.
// Static protocol actions keep the exchange deterministic while exercising each server's
// request decoder, action executor, response envelope, and the vendored hyper clock.
export async function checkRemainingHttpServers({ startServer, httpRequest, checkDate, fail }) {
    const samlMetadata = (role) => `<EntityDescriptor xmlns="urn:oasis:names:tc:SAML:2.0:metadata" entityID="urn:netget:smoke:${role}"><${role}SSODescriptor protocolSupportEnumeration="urn:oasis:names:tc:SAML:2.0:protocol"/></EntityDescriptor>`;
    const cases = [
        {
            protocol: 'yarn', path: '/ws/v1/cluster/apps',
            action: { type: 'send_yarn_apps', apps: [{ id: 'application_1_0001', name: 'browser-smoke', state: 'RUNNING' }] },
            check: (body) => JSON.parse(body).apps.app[0].name === 'browser-smoke',
        },
        {
            protocol: 'spark', path: '/api/v1/applications',
            action: { type: 'send_spark_applications', applications: [{ id: 'app-smoke', name: 'browser-smoke', attempts: [] }] },
            check: (body) => JSON.parse(body)[0].id === 'app-smoke',
        },
        {
            protocol: 'snowflake', path: '/queries/v1/query-request', method: 'POST',
            headers: { 'content-type': 'application/json', authorization: 'Snowflake Token="smoke-token"' },
            body: JSON.stringify({ sqlText: 'select 42', sequenceId: 1 }),
            action: { type: 'snowflake_query_response', query_id: 'browser-smoke', rowtype: [{ name: 'ANSWER', type: 'fixed' }], rowset: [[42]] },
            check: (body) => {
                const reply = JSON.parse(body);
                return reply.success === true && reply.data.queryId === 'browser-smoke' && reply.data.rowset[0][0] === '42';
            },
        },
        {
            protocol: 'mercurial', path: '/repo?cmd=heads',
            action: { type: 'hg_heads', heads: ['0123456789abcdef0123456789abcdef01234567'] },
            contentType: 'application/mercurial-0.1',
            check: (body) => body.trim() === '0123456789abcdef0123456789abcdef01234567',
        },
        {
            protocol: 'oci-registry', path: '/v2/_catalog',
            action: { type: 'send_oci_catalog', repositories: ['library/browser-smoke'] },
            check: (body) => JSON.parse(body).repositories[0] === 'library/browser-smoke',
            responseHeaders: { 'docker-distribution-api-version': 'registry/2.0' },
        },
        ...['idp', 'sp'].map((role) => ({
            protocol: `saml-${role}`, path: '/metadata',
            action: { type: 'send_metadata', metadata_xml: samlMetadata(role === 'idp' ? 'IDP' : 'SP') },
            contentType: 'application/samlmetadata+xml',
            check: (body) => body === samlMetadata(role === 'idp' ? 'IDP' : 'SP'),
        })),
        {
            protocol: 'kubernetes', path: '/api/v1/namespaces/default/pods',
            action: { type: 'k8s_list_response', kind: 'PodList', apiVersion: 'v1', items: [{ apiVersion: 'v1', kind: 'Pod', metadata: { name: 'browser-smoke', namespace: 'default' } }] },
            check: (body) => {
                const reply = JSON.parse(body);
                return reply.kind === 'PodList' && reply.items[0].metadata.name === 'browser-smoke';
            },
        },
    ];
    const checked = {};
    for (const [index, test] of cases.entries()) {
        const port = 8100 + index;
        await startServer({ protocol: test.protocol, port, event_handlers: [{ event_pattern: '*', handler: { type: 'static', actions: [test.action] } }] });
        const response = await httpRequest(port, { method: test.method || 'GET', path: test.path, headers: test.headers, body: test.body });
        if (response.status !== 200 || !test.check(response.body)) fail(`${test.protocol} ${test.path}: ${JSON.stringify(response)}`);
        const contentType = test.contentType || 'application/json';
        if (!String(response.headers['content-type']).startsWith(contentType)) fail(`${test.protocol} content-type: ${JSON.stringify(response.headers)}`);
        for (const [name, value] of Object.entries(test.responseHeaders || {})) {
            if (response.headers[name] !== value) fail(`${test.protocol} ${name}: ${JSON.stringify(response.headers)}`);
        }
        checkDate(response.headers.date, `${test.protocol} ${test.path}`);
        checked[test.protocol] = `${test.method || 'GET'} ${test.path} -> ${response.status}, ${response.body.length} bytes; envelope and Date checked`;
    }
    return checked;
}
