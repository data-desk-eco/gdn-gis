#!/bin/sh
# download the latest maps viewer bundle from mapsviewerportal.com, authenticating
# through dnv veracity (azure ad b2c, custom policy b2c_1a_signinwithadfsidp). the app
# drives a standard oidc auth-code-form_post dance, and the policy's journey is two
# self-asserted rounds — home-realm discovery (email) then the local account login
# (email + password) — each rendered as a b2c page carrying a SETTINGS blob (its `api`
# name + a csrf token) and a shared transaction id. we drive it headlessly:
#
#   /Home/LoginVeracity -> a self-posting form carrying the authorize request
#   authorize           -> b2c login page
#   repeat: SelfAsserted <- post the credentials ; <api>/confirmed -> next page
#   ...until confirmed hands back the self-posting form with code + id_token
#   /signin-oidc        <- post those; the app sets its session cookie
#   /DownloadZipFile    -> the bundle
# curl's cookie jar carries the b2c + app sessions; `postform` replays whichever
# auto-submit form b2c/asp.net returns (single- or double-quoted) without a browser.
#
# usage: fetch-maps.sh OUT USER PASS
set -e
out=${1:?out}; user=${2:?user}; pass=${3:?pass}
ua='Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36'
b2c=https://login.veracity.com/dnvglb2cprod.onmicrosoft.com/B2C_1A_SignInWithADFSIdp
jar=$(mktemp); trap 'rm -f "$jar" "$jar.f"' EXIT
c(){ curl -s -c "$jar" -b "$jar" -A "$ua" "$@"; }
attr(){ printf %s "$2" | sed -n "s/.*$1=[\"']\\([^\"']*\\)[\"'].*/\\1/p"; }         # first $1="v" / $1='v'

# post the first <form> in $1 to its action, honouring every hidden input (values may
# hold spaces / json — a curl config file passes them verbatim, no shell eval).
postform(){
  action=$(printf %s "$1" | grep -oiE "<form[^>]*action=[\"'][^\"']*[\"']" | head -1 | sed -E "s/.*action=[\"']//;s/[\"']$//")
  printf %s "$1" | grep -oiE '<input[^>]*>' | while IFS= read -r inp; do
    n=$(attr name "$inp"); v=$(attr value "$inp" | sed 's/&amp;/\&/g')
    [ -n "$n" ] && printf 'data-urlencode = "%s=%s"\n' "$n" "$v"
  done > "$jar.f" || :                                    # read's eof status must not trip set -e
  c -L -K "$jar.f" "$action"
}

# 1. trigger the oidc challenge, then replay it into the b2c login page
page=$(postform "$(c https://mapsviewerportal.com/Home/LoginVeracity)")
tx=$(printf %s "$page" | grep -oE 'StateProperties=[A-Za-z0-9_-]+' | head -1)

# 2. walk the self-asserted journey until confirmed returns the oidc token form
i=0
while ! printf %s "$page" | grep -qE "name=[\"']id_token[\"']"; do
  i=$((i + 1)); [ "$i" -gt 5 ] && { echo "fetch-maps: login did not complete (check credentials)" >&2; exit 1; }
  api=$(printf %s "$page" | grep -oE '"api":"[^"]*"' | head -1 | sed 's/.*:"//;s/"//')
  csrf=$(printf %s "$page" | grep -oE '"csrf":"[^"]*"' | head -1 | sed 's/.*:"//;s/"//')
  [ -n "$api" ] && [ -n "$csrf" ] && [ -n "$tx" ] || { echo "fetch-maps: could not parse the veracity login page" >&2; exit 1; }
  c "$b2c/SelfAsserted?tx=$tx&p=B2C_1A_SignInWithADFSIdp" \
    -H "x-csrf-token: $csrf" -H 'x-requested-with: XMLHttpRequest' \
    --data-urlencode request_type=RESPONSE \
    --data-urlencode "username=$user" --data-urlencode "password=$pass" >/dev/null
  page=$(c "$b2c/api/$api/confirmed?csrf_token=$csrf&tx=$tx&p=B2C_1A_SignInWithADFSIdp")
done

# 3. hand the code + id_token to the app, which sets its session cookie
postform "$page" >/dev/null

# 4. download the bundle
c -L -o "$out" https://mapsviewerportal.com/DownloadZipFile
echo "fetch-maps: wrote $out"
