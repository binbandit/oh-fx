use std::io::Write;
use std::os::fd::AsFd;

use ofx_text::encode_terminal_safe;
use tokio_util::sync::CancellationToken;

use super::{LoginTerminal, Team, VercelError, VercelOAuth, select_team, write_all};
use crate::oauth_session::Session;
use crate::subscription_access::now_ms;

impl VercelOAuth {
    pub async fn switch_team<F: AsFd>(
        &self,
        output: &mut dyn Write,
        input: &F,
        terminal: LoginTerminal,
    ) -> Result<(), VercelError> {
        let session = self.load_team_session().await?;
        let teams = self
            .fetch_teams(session.access_token.expose(), &session.issuer)
            .await?;
        let current = session.team_id.as_deref().or(session.team_slug.as_deref());
        let index = select_team(&teams, current, terminal.team_picker, input, output)?
            .ok_or(VercelError::NoTeams)?;
        let selected = &teams[index];
        self.save_team(&session, selected).await?;
        write_all(
            output,
            &format!(
                "Selected Vercel team: {} ({}).\n",
                encode_terminal_safe(selected.name.as_bytes(), usize::MAX).text,
                encode_terminal_safe(selected.slug.as_bytes(), usize::MAX).text
            ),
        )
    }

    async fn load_team_session(&self) -> Result<Session, VercelError> {
        let mutation = self
            .store()
            .begin_existing_mutation()
            .await?
            .ok_or(VercelError::NoSession)?;
        let mut session = mutation.load()?.ok_or(VercelError::NoSession)?;
        if session.expired(now_ms()) {
            self.refresh_session(&mutation, &mut session, &CancellationToken::new())
                .await?;
        }
        Ok(session)
    }

    async fn save_team(&self, session: &Session, team: &Team) -> Result<(), VercelError> {
        let mutation = self
            .store()
            .begin_existing_mutation()
            .await?
            .ok_or(VercelError::SessionChanged)?;
        let current = mutation.load()?.ok_or(VercelError::SessionChanged)?;
        if !same_credential_state(session, &current) {
            return Err(VercelError::SessionChanged);
        }
        mutation.save(&Session {
            team_id: Some(team.id.clone()),
            team_slug: Some(team.slug.clone()),
            ..current
        })
    }
}

fn same_credential_state(left: &Session, right: &Session) -> bool {
    left.expires_at_ms == right.expires_at_ms
        && left.issuer == right.issuer
        && left.client_id == right.client_id
        && left.access_token == right.access_token
        && left.refresh_token == right.refresh_token
        && left.scope == right.scope
        && left.token_type == right.token_type
}

#[cfg(test)]
mod tests;
