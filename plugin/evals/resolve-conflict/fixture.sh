source "$(dirname "${BASH_SOURCE[0]}")/../setup.sh"
# polite adds a greeting; loud, stacked on it, changes the same line. Then
# polite is amended, so restacking loud conflicts.
gh dags new polite >/dev/null
echo "hello there" > greeting.txt
git add greeting.txt
git commit -q -m "polite greeting"
gh dags new loud >/dev/null
echo "HELLO THERE" > greeting.txt
git commit -q -am "loud greeting"
git checkout -q polite
echo "good morning" > greeting.txt
git commit -q -am "politer greeting"
gh dags restack >/dev/null 2>&1 || true
