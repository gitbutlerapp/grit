export GIT_CONFIG_GLOBAL=/dev/null
mkdir /tmp/rn && cd /tmp/rn && git init -q -b main && git config user.name Dev && git config user.email d@e.x
seq 1 100 | sed 's/^/line /' > old.txt && git add . && git commit -qm one
git rm -q old.txt
(seq 1 45 | sed 's/^/line /'; seq 1 55 | sed 's/^/
