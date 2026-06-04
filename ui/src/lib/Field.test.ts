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
});
