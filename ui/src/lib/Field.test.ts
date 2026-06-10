import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import Field from './Field.svelte';

describe('Field', () => {
  it('renders the label and help text', () => {
    render(Field, { props: { label: 'Filename template', help: 'Use {date}_{original}' } });
    expect(screen.getByText('Filename template')).toBeTruthy();
    expect(screen.getByText('Use {date}_{original}')).toBeTruthy();
  });

  it('omits the help node when no help is given', () => {
    const { container } = render(Field, { props: { label: 'Verify' } });
    expect(screen.getByText('Verify')).toBeTruthy();
    expect(container.querySelector('.field-help')).toBeNull();
  });

  it('renders a <label for=…> when htmlFor is provided', () => {
    const { container } = render(Field, {
      props: { label: 'Filename template', htmlFor: 'tpl-input' },
    });
    const label = container.querySelector('label.field-label');
    expect(label).not.toBeNull();
    expect(label?.getAttribute('for')).toBe('tpl-input');
    expect(label?.textContent).toBe('Filename template');
    expect(container.querySelector('span.field-label')).toBeNull();
  });

  it('renders a non-label span when htmlFor is omitted (no orphan <label>)', () => {
    const { container } = render(Field, { props: { label: 'Verify' } });
    expect(container.querySelector('label')).toBeNull();
    const span = container.querySelector('span.field-label');
    expect(span).not.toBeNull();
    expect(span?.textContent).toBe('Verify');
  });
});
