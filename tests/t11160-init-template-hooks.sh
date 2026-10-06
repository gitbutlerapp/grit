#!/bin/sh
# Tests for grit init --template with hooks, info, and other template content.

test_description='grit init --template copies hooks and template content'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup template directories
###########################################################################

test_expect_success 'setup: create basic template with pre-commit hook' '
	mkdir -p tmpl-basic/hooks &&
	printf "#!/bin/sh\necho pre-commit\n" >tmpl-basic/hooks/pre-commit &&
	chmod +x tmpl-basic/hooks/pre-commit
'

test_expect_success 'setup: create template with multiple hooks' '
	mkdir -p tmpl-multi/hooks &&
	printf "#!/bin/sh\necho pre-commit\n" >tmpl-multi/hooks/pre-commit &&
	printf "#!/bin/sh\necho post-commit\n" >tmpl-multi/hooks/post-commit &&
	printf "#!/bin/sh\necho pre-push\n" >tmpl-multi/hooks/pre-push &&
	chmod +x tmpl-multi/hooks/*
'

test_expect_success 'setup: create template with info/exclude' '
	mkdir -p tmpl-info/info &&
	printf "*.log\n*.tmp\n" >tmpl-info/info/exclude
'

test_expect_success 'setup: create template with hooks and info' '
	mkdir -p tmpl-full/hooks tmpl-full/info &&
	printf "#!/bin/sh\necho hook\n" >tmpl-full/hooks/pre-commit &&
	chmod +x tmpl-full/hooks/pre-commit &&
	printf "*.bak\n" >tmpl-full/info/exclude
'

###########################################################################
# Section 2: Basic --template with hooks
###########################################################################

test_expect_success 'init --template copies pre-commit hook' '
	grit init --template tmpl-basic repo-basic &&
	test -f repo-basic/.git/hooks/pre-commit
'

test_expect_success 'init --template hook is executable' '
	test -x repo-basic/.git/hooks/pre-commit
'

test_expect_success 'init --template hook has correct content' '
	grep "echo pre-commit" repo-basic/.git/hooks/pre-commit
'

test_expect_success 'init --template produces valid repository' '
	cd repo-basic &&
	grit status >actual 2>&1 &&
	cd ..
'

test_expect_success 'init --template creates .git/objects' '
	test -d repo-basic/.git/objects
'

test_expect_success 'init --template creates .git/refs' '
	test -d repo-basic/.git/refs
'

test_expect_success 'init --template creates HEAD' '
	test -f repo-basic/.git/HEAD
'

###########################################################################
# Section 3: Multiple hooks
###########################################################################

test_expect_success 'init --template copies multiple hooks' '
	grit init --template tmpl-multi repo-multi &&
	test -f repo-multi/.git/hooks/pre-commit &&
	test -f repo-multi/.git/hooks/post-commit &&
	test -f repo-multi/.git/hooks/pre-push
'

test_expect_success 'init --template multiple hooks all executable' '
	test -x repo-multi/.git/hooks/pre-commit &&
	test -x repo-multi/.git/hooks/post-commit &&
	test -x repo-multi/.git/hooks/pre-push
'

test_expect_success 'init --template pre-push hook content correct' '
	grep "echo pre-push" repo-multi/.git/hooks/pre-push
'

test_expect_success 'init --template post-commit hook content correct' '
	grep "echo post-commit" repo-multi/.git/hooks/post-commit
'

test_expect_success 'init --template hook count matches template' '
	ls tmpl-multi/hooks/ | wc -l | tr -d " " >expect_count &&
	ls repo-multi/.git/hooks/ | wc -l | tr -d " " >actual_count &&
	test_cmp expect_count actual_count
'

###########################################################################
# Section 4: Template with info/exclude
###########################################################################

test_expect_success 'init --template copies info/exclude' '
	grit init --template tmpl-info repo-info &&
	test -f repo-info/.git/info/exclude
'

test_expect_success 'init --template info/exclude has correct content' '
	grep "\\*.log" repo-info/.git/info/exclude &&
	grep "\\*.tmp" repo-info/.git/info/exclude
'

test_expect_success 'init --template info/exclude content matches template' '
	test_cmp tmpl-info/info/exclude repo-info/.git/info/exclude
'

###########################################################################
# Section 5: Template with hooks and info combined
###########################################################################

test_expect_success 'init --template copies both hooks and info' '
	grit init --template tmpl-full repo-full &&
	test -f repo-full/.git/hooks/pre-commit &&
	test -f repo-full/.git/info/exclude
'

test_expect_success 'init --template full hook is executable' '
	test -x repo-full/.git/hooks/pre-commit
'

test_expect_success 'init --template full info has correct content' '
	grep "\\*.bak" repo-full/.git/info/exclude
'

###########################################################################
# Section 6: Template with absolute path
###########################################################################

test_expect_success 'init --template with absolute path' '
	abs_tmpl="$(pwd)/tmpl-basic" &&
	grit init --template "$abs_tmpl" repo-abs &&
	test -f repo-abs/.git/hooks/pre-commit
'

test_expect_success 'init --template absolute path hook is executable' '
	test -x repo-abs/.git/hooks/pre-commit
'

###########################################################################
# Section 7: Empty template
###########################################################################

test_expect_success 'init --template with empty template dir' '
	mkdir -p tmpl-empty &&
	grit init --template tmpl-empty repo-empty &&
	test -d repo-empty/.git
'

test_expect_success 'init --template empty template creates valid repo' '
	cd repo-empty &&
	grit status >actual 2>&1 &&
	cd ..
'

###########################################################################
# Section 8: Reinitializing with template
###########################################################################

test_expect_success 'init --template on existing repo does not destroy data' '
	grit init repo-reinit &&
	cd repo-reinit &&
	grit config user.name "Test" &&
	grit config user.email "test@test.com" &&
	echo "data" >file.txt &&
	grit add file.txt &&
	grit commit -m "initial" &&
	cd .. &&
	grit init --template tmpl-basic repo-reinit &&
	test -f repo-reinit/file.txt &&
	cd repo-reinit &&
	grit log --oneline >log_out &&
	grep "initial" log_out &&
	cd ..
'

test_expect_success 'init --template on existing repo copies hooks' '
	test -f repo-reinit/.git/hooks/pre-commit
'

###########################################################################
# Section 9: Compare with real git
###########################################################################

test_expect_success 'init --template hook matches real git template result' '
	"$REAL_GIT" init --template tmpl-basic repo-git-tmpl &&
	test -f repo-git-tmpl/.git/hooks/pre-commit &&
	diff repo-basic/.git/hooks/pre-commit repo-git-tmpl/.git/hooks/pre-commit
'

test_expect_success 'init --template info matches real git template result' '
	"$REAL_GIT" init --template tmpl-info repo-git-info &&
	test -f repo-git-info/.git/info/exclude &&
	diff repo-info/.git/info/exclude repo-git-info/.git/info/exclude
'

###########################################################################
# Section 10: Template with custom files
###########################################################################

test_expect_success 'setup: template with custom info exclude' '
	mkdir -p tmpl-excl/info &&
	echo "*.pyc" >tmpl-excl/info/exclude
'

test_expect_success 'init --template copies custom info exclude' '
	grit init --template tmpl-excl repo-excl &&
	test -f repo-excl/.git/info/exclude &&
	grep "\*.pyc" repo-excl/.git/info/exclude
'

test_expect_success 'init --template with -b sets initial branch' '
	grit init --template tmpl-basic -b main repo-branch &&
	cd repo-branch &&
	branch=$(grit branch --show-current) &&
	test "$branch" = "main" &&
	cd ..
'

test_expect_success 'init --template with -b still copies hooks' '
	test -f repo-branch/.git/hooks/pre-commit
'

test_expect_success 'init --template with --bare copies hooks' '
	grit init --bare --template tmpl-basic repo-bare.git &&
	test -f repo-bare.git/hooks/pre-commit
'

test_expect_success 'init --template bare hook is executable' '
	test -x repo-bare.git/hooks/pre-commit
'

test_done
