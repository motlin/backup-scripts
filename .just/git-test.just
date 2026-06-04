# set up `git-test`
setup-git-test:
    git test add --forget --test default 'just clean _check-local-modifications precommit'

git_tests := env('GIT_TESTS', "default")
