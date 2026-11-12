/**
 * snrust OAuth access diagnostics (READ-ONLY).
 *
 * Purpose: explain why ServiceNow issues snrust an OAuth token but then rejects REST calls
 * (HTTP 401/403 on /api/now/table/*). Run as an admin in System Definition > Scripts - Background
 * (scope: global) and paste the printed report back to whoever is troubleshooting.
 *
 * Reports: the snrust Application Registry (oauth_entity) settings, its auth scopes and the
 * REST APIs those scopes unlock, recently issued tokens and their granted scopes, active REST
 * API Access Policies, table web-service access, and recent OAuth/scope log lines.
 *
 * Never prints secrets: client_secret, token values/hashes, and session keys are skipped, and the
 * client_id is truncated. Performs no inserts, updates, or deletes.
 *
 * Matching: registries whose Redirect URL contains REDIRECT_HINT or whose name contains NAME_HINT.
 * Set CLIENT_ID_SUFFIX to the last characters of snrust's Client ID to pin one exact registry.
 */
(function snrustOAuthDiagnostics() {
    var REDIRECT_HINT = '127.0.0.1';
    var NAME_HINT = 'snrust';
    var CLIENT_ID_SUFFIX = '';
    var LOG_LOOKBACK_MINUTES = 120;

    var out = [];
    var findings = [];

    function p(line) { out.push(line); }
    function hr(title) { p(''); p('=== ' + title + ' ==='); }
    function val(gr, field) {
        if (!gr.isValidField(field)) return '(n/a)';
        var raw = gr.getValue(field);
        var shown = gr.getDisplayValue(field);
        if (raw === null || raw === '') return '(empty)';
        return shown && shown != raw ? shown + ' [' + raw + ']' : String(raw);
    }
    function tail(s, n) { s = String(s || ''); return s.length > n ? '...' + s.slice(-n) : s; }
    function clip(s, n) { s = String(s || '').replace(/\s+/g, ' '); return s.length > n ? s.slice(0, n) + '...' : s; }

    hr('Instance & current user');
    p('build: ' + gs.getProperty('glide.buildname', '?') + ' / ' + gs.getProperty('glide.war', '?'));
    var me = new GlideRecord('sys_user');
    if (me.get(gs.getUserID())) {
        p('user: ' + me.getValue('user_name') + ' (' + me.getDisplayValue() + ')');
        ['active', 'locked_out', 'web_service_access_only', 'internal_integration_user'].forEach(function (f) {
            p('  ' + f + ': ' + val(me, f));
        });
    }
    p('  has admin: ' + gs.hasRole('admin') + ', has itil: ' + gs.hasRole('itil'));

    hr('Application Registry (oauth_entity) candidates');
    var ent = new GlideRecord('oauth_entity');
    var q = ent.addQuery('redirect_url', 'CONTAINS', REDIRECT_HINT);
    q.addOrCondition('name', 'CONTAINS', NAME_HINT);
    ent.orderByDesc('sys_updated_on');
    ent.setLimit(10);
    ent.query();

    var matched = 0;
    var seenEntities = [];
    var entityFields = [
        'active', 'type', 'access', 'public_client', 'use_pkce', 'code_challenge_method',
        'inbound_grant_type', 'client_type', 'scope_restriction_status', 'restricted_binding',
        'only_rest_processor', 'bind_token_to_session', 'token_format', 'skip_authorize',
        'redirect_url', 'access_token_lifespan', 'refresh_token_lifespan', 'user', 'sys_scope',
        'sys_created_on', 'sys_updated_on'
    ];
    while (ent.next()) {
        var clientId = ent.getValue('client_id') || '';
        if (CLIENT_ID_SUFFIX && clientId.slice(-CLIENT_ID_SUFFIX.length) != CLIENT_ID_SUFFIX) continue;
        matched++;
        var entName = ent.getValue('name');
        p('');
        p('-- ' + entName + '  sys_id=' + ent.getUniqueValue() + '  client_id=' + tail(clientId, 6));
        entityFields.forEach(function (f) { p('  ' + f + ': ' + val(ent, f)); });

        var scopeNames = [];
        var map = new GlideRecord('oauth_entity_auth_scope_mapping');
        if (map.isValid()) {
            map.addQuery('oauth_entity', ent.getUniqueValue());
            map.query();
            while (map.next()) scopeNames.push({ id: map.getValue('auth_scope'), name: map.getDisplayValue('auth_scope') });
        }
        p('  Auth Scopes (oauth_entity_auth_scope_mapping): ' +
            (scopeNames.length ? scopeNames.map(function (s) { return s.name; }).join(', ') : '(none)'));

        var legacy = [];
        var ls = new GlideRecord('oauth_entity_scope');
        if (ls.isValid()) {
            ls.addQuery('oauth_entity', ent.getUniqueValue());
            ls.query();
            while (ls.next()) legacy.push(ls.getValue('oauth_entity_scope') + (ls.getValue('in_default_profile') == '1' ? ' (default profile)' : ''));
        }
        p('  OAuth Entity Scopes (oauth_entity_scope): ' + (legacy.length ? legacy.join(', ') : '(none)'));

        var hasUserAccount = scopeNames.some(function (s) { return s.name == 'useraccount'; }) ||
            legacy.some(function (s) { return s.indexOf('useraccount') === 0; });
        seenEntities.push({
            name: entName,
            scopeIds: scopeNames.map(function (s) { return s.id; }),
            hasUserAccount: hasUserAccount
        });
        var tableApiUnlocked = false;
        scopeNames.forEach(function (s) {
            if (s.name == 'useraccount') return;
            var api = new GlideRecord('sys_api_access_scope');
            if (!api.isValid()) return;
            api.addQuery('auth_scope', s.id);
            api.addActiveQuery();
            api.query();
            while (api.next()) {
                var apiName = api.getDisplayValue('api') + ' ' + (api.getValue('api_path') || '');
                if (/table/i.test(apiName)) tableApiUnlocked = true;
                p('    scope ' + s.name + ' -> REST API: ' + clip(apiName, 80) +
                    ' | all resources=' + api.getValue('apply_all_resources') +
                    ' | method=' + (api.getValue('apply_all_methods') == '1' ? 'ALL' : api.getDisplayValue('http_method')));
            }
        });

        p('  Recent tokens issued to this registry (no token values):');
        var cred = new GlideRecord('oauth_credential');
        cred.addQuery('peer', ent.getUniqueValue());
        cred.orderByDesc('sys_created_on');
        cred.setLimit(6);
        cred.query();
        var tokenCount = 0;
        while (cred.next()) {
            tokenCount++;
            p('    ' + cred.getValue('sys_created_on') + ' type=' + cred.getValue('type') +
                ' user=' + cred.getDisplayValue('user') + ' scopes=' + (cred.getValue('scopes') || '(empty)') +
                ' grant=' + (cred.getValue('grant_type') || '?') + ' expires=' + cred.getValue('expires') +
                ' session_bound=' + (cred.getValue('bound_session') ? 'yes' : 'no'));
        }
        if (!tokenCount) p('    (none)');

        var restriction = ent.getValue('scope_restriction_status');
        if (restriction == 'restricted' && !tableApiUnlocked) {
            findings.push('[' + entName + '] Scope Restriction is "Securely scoped" and none of its auth scopes unlock the Table API -> every /api/now/table call returns 403. Set Scope Restriction to "Broadly scoped" (or "Useraccount scoped" with the useraccount scope), then sign in again.');
        }
        if (ent.getValue('restricted_binding') == '1') {
            findings.push('[' + entName + '] "Enforce Token Restrictions" is checked -> tokens only work for APIs mapped to its auth scopes. Uncheck it (or map the Table API), then sign in again.');
        }
        if (!hasUserAccount) {
            findings.push('[' + entName + '] Auth scopes do not include "useraccount" (snrust requests scope=useraccount). Add the useraccount auth scope, then sign in again.');
        }
        if (ent.getValue('bind_token_to_session') == '1') {
            findings.push('[' + entName + '] "Bind Access Token to Session" is checked -> tokens only work alongside the browser session that created them. Uncheck it for a desktop client.');
        }
        if (ent.getValue('only_rest_processor') == '1') {
            p('  note: "Only for REST Endpoints" is checked (fine for Table API; blocks UI processors such as /amb).');
        }
        if (ent.getValue('active') != '1') {
            findings.push('[' + entName + '] Registry is inactive.');
        }
    }
    if (!matched) {
        p('No registry matched redirect "' + REDIRECT_HINT + '" / name "' + NAME_HINT + '". Edit REDIRECT_HINT/NAME_HINT/CLIENT_ID_SUFFIX at the top and re-run.');
        findings.push('No snrust Application Registry found by the script filters.');
    }

    hr('Active REST API Access Policies (sys_api_access_policy)');
    var pol = new GlideRecord('sys_api_access_policy');
    if (pol.isValid()) {
        pol.addActiveQuery();
        pol.setLimit(40);
        pol.query();
        var polCount = 0;
        while (pol.next()) {
            polCount++;
            var apiLabel = pol.getDisplayValue('api') + ' ' + (pol.getValue('api_path') || '');
            var profiles = [];
            var pm = new GlideRecord('sys_auth_profile_mapping');
            if (pm.isValid()) {
                pm.addQuery('api_access_policy', pol.getUniqueValue());
                pm.query();
                while (pm.next()) profiles.push(pm.getDisplayValue('inbound_auth_profile'));
            }
            var global = pol.getValue('apply_all') == '1' || pol.getValue('apply_global') == '1';
            p('- ' + pol.getValue('name') + ' | api=' + clip(apiLabel, 60) +
                ' | apply_all=' + pol.getValue('apply_all') + ' apply_global=' + pol.getValue('apply_global') +
                ' all_tables=' + pol.getValue('apply_all_tables') + ' tables=' + (pol.getValue('api_tablename') || '-') +
                ' | auth profiles: ' + (profiles.length ? profiles.join(', ') : '(none)'));
            if ((global || /table/i.test(apiLabel)) && profiles.length &&
                !profiles.some(function (n) { return /oauth/i.test(n); })) {
                findings.push('API Access Policy "' + pol.getValue('name') + '" covers the Table API but allows no OAuth inbound auth profile.');
            }
        }
        if (!polCount) p('(no active policies)');
    }

    // ServiceNow answers 403 "Missing required api access scope" when an API is mapped to an auth
    // scope (sys_api_access_scope) that the token's registry lacks. "useraccount" bypasses that
    // check, but only while glide.oauth.token.scope.useraccount points at the real useraccount record.
    hr('REST API Auth Scope gate (sys_api_access_scope / properties)');
    var checkEnabled = gs.getProperty('com.glide.rest.api.auth.scope.check.enable', '(unset, default true)');
    p('com.glide.rest.api.auth.scope.check.enable: ' + checkEnabled);
    var uaPropId = gs.getProperty('glide.oauth.token.scope.useraccount', '');
    var uaScope = new GlideRecord('sys_auth_scope');
    var uaOk = !!uaPropId && uaScope.get(uaPropId) && uaScope.getValue('name') == 'useraccount';
    p('glide.oauth.token.scope.useraccount: ' + (uaPropId || '(empty)') +
        (uaOk ? ' -> useraccount (ok)' : ' -> does NOT resolve to the useraccount auth scope'));
    if (!uaOk && String(checkEnabled) != 'false') {
        findings.push('glide.oauth.token.scope.useraccount does not point at the "useraccount" sys_auth_scope record, so useraccount tokens no longer bypass REST API Auth Scope checks. Set it to the useraccount record\'s sys_id.');
    }

    var tableApiScopeIds = [];
    var apiScope = new GlideRecord('sys_api_access_scope');
    if (apiScope.isValid()) {
        if (apiScope.isValidField('active')) apiScope.addActiveQuery();
        apiScope.setLimit(100);
        apiScope.query();
        while (apiScope.next()) {
            var scopedApi = apiScope.getDisplayValue('api') + ' ' + (apiScope.getValue('api_path') || '');
            if (!/table/i.test(scopedApi)) continue;
            tableApiScopeIds.push(apiScope.getValue('auth_scope'));
            p('- Table API requires auth scope "' + apiScope.getDisplayValue('auth_scope') + '" | api=' + clip(scopedApi, 60) +
                ' | all resources=' + apiScope.getValue('apply_all_resources') +
                ' | method=' + (apiScope.getValue('apply_all_methods') == '1' ? 'ALL' : apiScope.getDisplayValue('http_method')));
        }
        if (!tableApiScopeIds.length) p('(no REST API Auth Scope records target the Table API; any OAuth token may call it)');
    } else {
        p('(sys_api_access_scope not present on this instance)');
    }
    if (tableApiScopeIds.length && String(checkEnabled) != 'false') {
        seenEntities.forEach(function (e) {
            var granted = e.scopeIds.some(function (id) { return tableApiScopeIds.indexOf(id) >= 0; });
            if (!granted && !e.hasUserAccount) {
                findings.push('[' + e.name + '] The Table API is gated by REST API Auth Scope, but this registry has neither "useraccount" nor a scope mapped to the Table API -> 403 "Missing required api access scope". Add the useraccount auth scope to the registry, then sign in again.');
            }
        });
    }

    hr('Table web-service access (sys_db_object.ws_access)');
    var tbl = new GlideRecord('sys_db_object');
    tbl.addQuery('name', 'IN', 'incident,task,sys_user,sys_db_object,sys_dictionary,sys_choice');
    tbl.query();
    while (tbl.next()) {
        var ws = tbl.isValidField('ws_access') ? tbl.getValue('ws_access') : '(n/a)';
        p('- ' + tbl.getValue('name') + ': ws_access=' + ws);
        if (ws == '0') findings.push('Table ' + tbl.getValue('name') + ' has "Allow access to this table via web services" unchecked.');
    }

    hr('Recent OAuth / scope / 403 log lines (last ' + LOG_LOOKBACK_MINUTES + ' min)');
    var log = new GlideRecord('syslog');
    log.addQuery('sys_created_on', '>=', gs.minutesAgoStart(LOG_LOOKBACK_MINUTES));
    var lq = log.addQuery('message', 'CONTAINS', 'oauth');
    lq.addOrCondition('message', 'CONTAINS', 'scope');
    lq.addOrCondition('message', 'CONTAINS', 'token');
    lq.addOrCondition('message', 'CONTAINS', 'Not Authorized');
    log.orderByDesc('sys_created_on');
    log.setLimit(25);
    log.query();
    var logCount = 0;
    while (log.next()) {
        logCount++;
        p('- ' + log.getValue('sys_created_on') + ' [' + log.getDisplayValue('level') + '] ' +
            clip(log.getValue('source'), 30) + ': ' + clip(log.getValue('message'), 260));
    }
    if (!logCount) p('(none)');

    hr('Likely causes');
    if (findings.length) findings.forEach(function (f, i) { p((i + 1) + '. ' + f); });
    else p('No configuration problem detected by this script. Share the snrust error text (it now includes ServiceNow\'s detail) and ~/Library/Logs/snrust/snrust.log.');

    gs.print('\n' + out.join('\n'));
})();
